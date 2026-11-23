//! A symbol-naming attribute's value written as a macro call: `#[link_name = concat!(prefix!(),
//! stringify!(f))]` (ring 0.17.14 src/prefixed.rs, on every item `prefixed_extern!` declares).
//!
//! The Reference, "Attributes", "Meta Item Attribute Syntax": "Expressions in meta items must
//! macro-expand to literal expressions, which must not include integer or float type suffixes."
//!
//! The lowering keeps each such value of a `link_name` or `export_name` as a [`ValueSite`]: the
//! item, the attribute, the module and the item's place in its module's textual sequence, and
//! the value's tokens (lexed from a file's text, or a range of the sealed tokens of the
//! expansion that wrote the item, so a `$crate` keeps its mark). Once the crate's names are
//! solved, [`value_rows`] expands every value AT THE ITEM'S PLACE:
//!
//! - A call's path is walked as a call written there would be ([`crate::names::path`] in the
//!   macro namespace): the item's module's `macro_rules!` cut at the item (a definition written
//!   after the item is not in its textual scope), its ancestors', the module's bindings, the
//!   `#[macro_use]` prelude, a dependency's exported macros (each read of a dependency a question
//!   of the crate's read list, `crate::asked`).
//! - A builtin that asks for its input's calls first (`concat!`) has each one expanded the same
//!   way, every result sealed by its call's key, and is entered again with the answers.
//! - A result that is again one call (`prefix!()` gives `concat!(core_name_and_version!(),
//!   "_")`) is expanded in turn, at most [`EAGER_DEPTH`] calls deep, so a macro that writes itself
//!   is refused by name, never followed.
//! - The last form must be ONE string literal with no suffix: its value is the row's.
//!
//! Every refusal is a typed [`ValueWhy`] naming the macro it stopped at and the last form
//! reached; no value is guessed or read another way (rule 3). The rows live with the crate's
//! names (`CrateNames::values`), sorted by item and attribute: the native image's link table
//! (blaze-ladder front2 links.rs) reads them by item key and never expands a value again (rule
//! 77). Each row keeps what its value read (the macros' sources, `env!`'s variables).

#![blaze::sans_io]

use core::fmt::{self, Write as _};

use blaze_expand::origin_column::TextMark;
use blaze_expand::{
    Call, CallKind, CallSite, Ctx, Delim, Eager, ExpandCx, ExpandError, Expanded, Failed, Inside, LexError, LitKind, NestedKind, PathScope, ReadAnswer,
    ReadKind, StreamError, TokenKind, TokenStream, attr_literal, call_key, expand, lex, write_token, write_tokens,
};
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{AString, AVec, Alloc, BString, BVec, Bump};
use blaze_surface::{Binding, CallShape, CrateRootBuilder, CrateRootHead, DefKind, Edition, Globs, MacroShapes, Namespace, RefusedWhy, ScopeBuilder};

use crate::asked::Question;
use crate::crate_scope::Dep;
use crate::names::{PathError, RootRead, ScopeRead, VariantHit, Why, World, Written, variant_in};
use crate::nested_input::{CallTokens, NestedInputError};
use crate::resolve::{Built, CrateNames, EAGER_DEPTH, definition_of};
use crate::task::{Loc, to_path_head};

/// One attribute value written as a macro call, as the lowering found it.
#[derive(Clone, Copy, Debug)]
pub struct ValueSite<'t> {
    /// The item the attribute is on.
    pub item: ItemKey,
    /// `"link_name"` or `"export_name"`.
    pub attr: &'static str,
    /// The module the item is declared in.
    pub module: ItemKey,
    /// The item's place: its fragment's owner (a module's key, or the key of the call whose
    /// expansion wrote it) and the last ordered entry's place before it there, as a body's
    /// place is kept (`BodySrc::place`); the names solve counts the module's textual sequence
    /// there (`Solved::cuts`).
    pub place: (ItemKey, u32),
    /// Where the item is, for a refusal's words and the host's `line!()`.
    pub loc: Loc,
    /// The file the item's text is written in (`CallSrc::written`).
    pub written: &'t str,
    /// The value as written (after the `=`, trimmed).
    pub value: &'t str,
    /// Where the value's tokens come from: the file's text, or the expansion's sealed tokens.
    pub tokens: CallTokens<'t>,
}

/// Whether an attribute value as written (`expr` after the `=`) is a macro call: a `!` before
/// any `"` (`"a!b"` is a string, `concat!("a")` a call). The lowering's rule
/// (`crate::task::attr_string`), said once here for the readers of [`ValueRow`]s.
pub fn is_call(expr: &str) -> bool {
    match (expr.find('!'), expr.find('"')) {
        (Some(bang), quote) => quote.is_none_or(|q| bang < q),
        (None, _) => false,
    }
}

/// Why a value is no symbol.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ValueWhy<'a> {
    /// The value's text did not lex.
    Tokens { lex: LexError },
    /// The value's tokens, a range of the expansion that wrote the item, did not copy.
    Input { error: StreamError },
    /// The value is written in an expansion, yet is no range of its tokens.
    Nested(NestedInputError),
    /// The names solve counted no textual scope at the item's place: where the value's macros
    /// are looked up is not known.
    NoCut,
    /// Two different values of one attribute on one item.
    TwoSites,
    /// The macro's path names nothing at the item's place (`why`: the walk's own reason).
    NotFound { mac: &'a str, why: PathError },
    /// The path names a definition whose source no crate read holds.
    NoSource { mac: &'a str },
    /// The definition did not build (`note`: the expansion note, as debug text).
    Definition { mac: &'a str, note: &'a str },
    Expand { mac: &'a str, error: ExpandError },
    /// The call expanded to an error (no rule matched, `env!` of a variable not set, ..).
    Failed { mac: &'a str, failed: Failed<'a> },
    /// A builtin asked for an input's macro definition (`macro_rules!`) to be expanded.
    NotACall { mac: &'a str },
    /// A builtin re-entered with its answers asked again.
    AskedAgain { mac: &'a str },
    /// The value is still a call [`EAGER_DEPTH`] calls deep.
    TooDeep { mac: &'a str },
    /// A call's input did not copy out of the stream it is written in.
    Stream { mac: &'a str, error: StreamError },
    /// The last form is no single string literal.
    NotAString,
    /// The string literal has a suffix (the Reference forbids it).
    Suffixed,
    /// The literal's escapes did not read.
    Escape,
}

impl fmt::Display for ValueWhy<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValueWhy::Tokens { lex } => write!(f, "the value did not lex: {lex}"),
            ValueWhy::Input { error } => write!(f, "the value's tokens did not copy out of the expansion that wrote the item: {error}"),
            ValueWhy::Nested(error) => write!(f, "{error}"),
            ValueWhy::NoCut => f.write_str("the names solve counted no textual scope at the item's place"),
            ValueWhy::TwoSites => f.write_str("the attribute is given twice with different values"),
            ValueWhy::NotFound { mac, why: PathError { why: Why::NotFound, .. } } => write!(f, "cannot find macro `{mac}` in this scope"),
            ValueWhy::NotFound { mac, why } => write!(f, "the macro `{mac}`: {why}"),
            ValueWhy::NoSource { mac } => write!(f, "the macro `{mac}`: no crate read holds its source"),
            ValueWhy::Definition { mac, note } => write!(f, "the macro `{mac}`'s definition did not build: {note}"),
            ValueWhy::Expand { mac, error } => write!(f, "`{mac}!` did not expand: {error}"),
            ValueWhy::Failed { mac, failed } => write!(f, "`{mac}!` failed: {failed}"),
            ValueWhy::NotACall { mac } => write!(f, "`{mac}!` asked for a definition in its input to be expanded"),
            ValueWhy::AskedAgain { mac } => write!(f, "`{mac}!` asked for its input's calls again after their answers"),
            ValueWhy::TooDeep { mac } => write!(f, "`{mac}!` is still a macro call {EAGER_DEPTH} calls deep"),
            ValueWhy::Stream { mac, error } => write!(f, "the input of `{mac}!` did not copy: {error}"),
            ValueWhy::NotAString => f.write_str("the value expanded to no single string literal"),
            ValueWhy::Suffixed => f.write_str("the value's string literal has a suffix"),
            ValueWhy::Escape => f.write_str("the value's string literal has an escape that does not read"),
        }
    }
}

/// A value refused: the item, the attribute, why, and the last form reached (the tokens being
/// expanded or read when it stopped, rendered).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ValueRefused<'a> {
    pub item: ItemKey,
    pub attr: &'static str,
    pub why: ValueWhy<'a>,
    pub form: &'a str,
}

impl fmt::Display for ValueRefused<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "#[{} = ..] on item {:x}: {} (the last form reached: `{}`; the Reference, \"Meta Item Attribute Syntax\": a value must macro-expand to a literal)",
            self.attr, self.item, self.why, self.form
        )
    }
}

/// What a value read: a macro's source (its digest, `crate::asked::macro_digest`), or a fact of
/// the host (an `env!` variable, as blaze-expand records it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ValueRead<'a> {
    Macro { def: ItemKey, digest: ContentId },
    Host { kind: ReadKind, subject: &'a str, answer: ReadAnswer },
}

/// One attribute's value on one item: the string, or why there is none, and what it read, in
/// the order read, each once.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ValueRow<'a> {
    pub item: ItemKey,
    pub attr: &'static str,
    pub value: Result<&'a str, ValueRefused<'a>>,
    pub reads: &'a [ValueRead<'a>],
}

/// The row of `attr` on `item` among `rows` (sorted by item, then attribute, as
/// [`value_rows`] gives them).
pub fn row<'r, 'a>(rows: &'r [ValueRow<'a>], item: ItemKey, attr: &str) -> Option<&'r ValueRow<'a>> {
    rows.binary_search_by(|r| (r.item, r.attr).cmp(&(item, attr))).ok().map(|i| &rows[i])
}

/// The attributes whose value may be written as a macro call and is kept as a row, in the
/// order of their byte in the items record's value table (crate::items_record::values): the
/// strings' own order, so the record's rows and [`row`]'s sort alike.
pub const ATTRS: [&str; 2] = ["export_name", "link_name"];

/// A row's refusal as a reader gets it: the solve's own typed refusal, or the words the
/// crate's items record keeps of it (its `Display`, written once with the record).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowRefusal<'a> {
    Typed(ValueRefused<'a>),
    Recorded(&'a str),
}

impl fmt::Display for RowRefusal<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RowRefusal::Typed(refused) => write!(f, "{refused}"),
            RowRefusal::Recorded(words) => f.write_str(words),
        }
    }
}

/// What one row answers: the value's string, or its refusal. The ONE answer to "the value of
/// this attribute on this item" for every reader (the link table's walk at the solve, the code
/// side over the solve or over the record: blaze-ladder front2 links.rs `symbol_of`).
pub type RowValue<'a> = Result<&'a str, RowRefusal<'a>>;

impl<'a> ValueRow<'a> {
    /// The row's answer.
    pub fn answer(&self) -> RowValue<'a> {
        self.value.map_err(RowRefusal::Typed)
    }
}

/// Every value site of crate `own` expanded at its item's place: one row per item and
/// attribute, sorted. `cuts` is the solve's count of each place's textual sequence
/// (`Solved::cuts`, sorted by place); `cx_of` the expansion host of a site (its file, its
/// module's path, the crate's environment).
pub(crate) fn value_rows<'a, C: ExpandCx>(
    own: &CrateNames<'a>,
    built: &Built<'_, 'a>,
    cuts: &[((ItemKey, u32), u32)],
    cx_of: &dyn Fn(&ValueSite<'a>) -> C,
    alloc: Bump<'a>,
) -> AVec<'a, ValueRow<'a>> {
    let a = alloc.alloc();
    let mut sites: AVec<'a, ValueSite<'a>> = AVec::with_capacity_in(own.tables.values.len(), a);
    sites.extend(own.tables.values.iter().copied());
    sites.sort_unstable_by(|a, b| (a.item, a.attr, a.value).cmp(&(b.item, b.attr, b.value)));
    let mut rows: AVec<'a, ValueRow<'a>> = AVec::with_capacity_in(sites.len(), a);
    let mut i = 0;
    while i < sites.len() {
        let site = sites[i];
        let mut end = i + 1;
        let mut two = false;
        while end < sites.len() && (sites[end].item, sites[end].attr) == (site.item, site.attr) {
            two |= sites[end].value != site.value;
            end += 1;
        }
        let row = if two {
            let why = ValueRefused { item: site.item, attr: site.attr, why: ValueWhy::TwoSites, form: site.value };
            ValueRow { item: site.item, attr: site.attr, value: Err(why), reads: &[] }
        } else {
            value_of(own, built, cuts, &cx_of(&site), site, alloc)
        };
        rows.push(row);
        i = end;
    }
    rows
}

/// One site's row.
fn value_of<'a, C: ExpandCx>(own: &CrateNames<'a>, built: &Built<'_, 'a>, cuts: &[((ItemKey, u32), u32)], cx: &C, site: ValueSite<'a>, alloc: Bump<'a>) -> ValueRow<'a> {
    let Some(cut) = cuts.binary_search_by_key(&site.place, |c| c.0).ok().map(|i| cuts[i].1) else {
        let why = ValueRefused { item: site.item, attr: site.attr, why: ValueWhy::NoCut, form: site.value };
        return ValueRow { item: site.item, attr: site.attr, value: Err(why), reads: &[] };
    };
    let mut driver = Driver {
        world: ValueWorld { own, built, module: site.module, cut, alloc: alloc.alloc() },
        bump: alloc,
        cx,
        edition: own.edition,
        item: site.item,
        attr: site.attr,
        reads: BVec::new_in(alloc),
        form: site.value,
    };
    let value = driver.value(site);
    let reads: &'a [ValueRead<'a>] = driver.reads.into_bump_slice();
    ValueRow { item: site.item, attr: site.attr, value, reads }
}

/// `text` kept as long as `alloc`.
fn keep<'a>(text: &str, alloc: Bump<'a>) -> &'a str {
    alloc.alloc_str(text)
}

// ---------------------------------------------------------------------------------------------
// The names at the item's place.

/// The crate's finished names and the crates it reads, as a call written at the site reads
/// them: the site's module's textual sequence is cut at the item.
struct ValueWorld<'q, 'r, 'a> {
    own: &'q CrateNames<'a>,
    built: &'q Built<'r, 'a>,
    /// The site's module, and the length of its textual sequence at the item.
    module: ItemKey,
    cut: u32,
    alloc: Alloc<'a>,
}

impl<'a> ValueWorld<'_, '_, 'a> {
    fn own(&self, key: ItemKey) -> bool {
        key.crate_prefix() == self.own.key.prefix()
    }

    /// A dependency's root, its read noted (`Question::krate`: the crate's facts).
    fn dep_root(&self, dep: Dep<'a>) -> ValueRoot<'_, 'a> {
        self.built.ask(|| Question::krate(dep, self.alloc));
        // The walk reads a dependency's root only for `$crate` (its root module) and its
        // exported macros: the head's other facts are the crate's own walk's, never read here.
        let head = CrateRootHead {
            krate: dep.key(),
            root_module: dep.root(),
            root_file: ContentId([0; 32]),
            edition: dep.edition(),
            no_std: false,
            no_core: false,
            arbitrary_ints: false,
            prelude: None,
        };
        ValueRoot::Dep { head, built: self.built, alloc: self.alloc }
    }
}

/// A module's scope as [`ValueWorld`] reads it.
#[derive(Clone, Copy)]
enum ValueScope<'w, 'a> {
    /// A module of the crate; `cut`: the site's module, its textual sequence cut at the item.
    Own { scope: &'w ScopeBuilder<'w, &'a str>, cut: Option<u32> },
    /// A module of a dependency: each name its whole answer there ([`Built::lookup`], its own
    /// binding then its globs), noted as a question of the read list.
    Dep { built: &'w Built<'w, 'a>, module: ItemKey, alloc: Alloc<'a> },
}

/// A crate's root as [`ValueWorld`] reads it.
#[derive(Clone, Copy)]
enum ValueRoot<'w, 'a> {
    Own(&'w CrateRootBuilder<'w, &'a str>),
    Dep { head: CrateRootHead, built: &'w Built<'w, 'a>, alloc: Alloc<'a> },
}

/// `name` in `ns` of a dependency's `module`, its answer noted.
fn dep_lookup<'a>(built: &Built<'_, 'a>, module: ItemKey, ns: Namespace, name: &'a str, alloc: Alloc<'a>) -> Option<Binding> {
    let found = built.lookup(module, ns, name, alloc);
    built.ask(|| Question::lookup(module, ns, name, found, alloc));
    found
}

impl<'w, 'a> ScopeRead<'w, &'a str> for ValueScope<'w, 'a> {
    fn get(self, ns: Namespace, name: &'a str) -> Option<Binding> {
        match self {
            ValueScope::Own { scope, .. } => <&'w ScopeBuilder<'w, &'a str> as ScopeRead<'w, &'a str>>::get(scope, ns, name),
            ValueScope::Dep { built, module, alloc } => dep_lookup(built, module, ns, name, alloc),
        }
    }

    fn globs(self) -> Globs<'w> {
        match self {
            ValueScope::Own { scope, .. } => <&'w ScopeBuilder<'w, &'a str> as ScopeRead<'w, &'a str>>::globs(scope),
            // A dependency's globs are in its lookup's answer.
            ValueScope::Dep { .. } => Globs::of(&[]),
        }
    }

    fn refused(self) -> Option<(ItemKey, RefusedWhy)> {
        match self {
            ValueScope::Own { scope, .. } => <&'w ScopeBuilder<'w, &'a str> as ScopeRead<'w, &'a str>>::refused(scope),
            ValueScope::Dep { .. } => None,
        }
    }

    fn textual(self, child: Option<ItemKey>, name: &'a str) -> Option<(ItemKey, MacroShapes)> {
        match (self, child) {
            // The site's own module: the definitions before the item (the Reference, "Macros
            // By Example", "Textual scope": a `macro_rules!` is in scope after it is defined).
            (ValueScope::Own { scope, cut: Some(cut) }, None) => {
                let end = (cut as usize).min(scope.textual.len());
                scope.textual[..end].iter().rev().find(|d| d.name == name).map(|d| (d.def, d.shapes))
            }
            (ValueScope::Own { scope, .. }, child) => <&'w ScopeBuilder<'w, &'a str> as ScopeRead<'w, &'a str>>::textual(scope, child, name),
            // A walk never enters a dependency's textual scope.
            (ValueScope::Dep { .. }, _) => None,
        }
    }

    fn marked(self, child: ItemKey) -> bool {
        match self {
            ValueScope::Own { scope, .. } => <&'w ScopeBuilder<'w, &'a str> as ScopeRead<'w, &'a str>>::marked(scope, child),
            ValueScope::Dep { .. } => false,
        }
    }

    fn each_trait(self, each: &mut dyn FnMut(ItemKey)) {
        if let ValueScope::Own { scope, .. } = self {
            <&'w ScopeBuilder<'w, &'a str> as ScopeRead<'w, &'a str>>::each_trait(scope, each);
        }
    }
}

impl<'w, 'a> RootRead<&'a str> for ValueRoot<'w, 'a> {
    fn head(self) -> CrateRootHead {
        match self {
            ValueRoot::Own(root) => <&'w CrateRootBuilder<'w, &'a str> as RootRead<&'a str>>::head(root),
            ValueRoot::Dep { head, .. } => head,
        }
    }

    fn extern_crate(self, name: &'a str) -> Option<ItemKey> {
        match self {
            ValueRoot::Own(root) => <&'w CrateRootBuilder<'w, &'a str> as RootRead<&'a str>>::extern_crate(root, name),
            ValueRoot::Dep { .. } => None,
        }
    }

    fn macro_use(self, i: usize) -> Option<ItemKey> {
        match self {
            ValueRoot::Own(root) => <&'w CrateRootBuilder<'w, &'a str> as RootRead<&'a str>>::macro_use(root, i),
            ValueRoot::Dep { .. } => None,
        }
    }

    /// A dependency's `#[macro_export]` macro: bound in its root module's macro namespace.
    fn macro_export(self, name: &'a str) -> Option<(ItemKey, MacroShapes)> {
        match self {
            ValueRoot::Own(root) => <&'w CrateRootBuilder<'w, &'a str> as RootRead<&'a str>>::macro_export(root, name),
            ValueRoot::Dep { head, built, alloc } => {
                let found = dep_lookup(built, head.root_module, Namespace::Macros, name, alloc)?;
                match found.def.kind {
                    DefKind::Macro(shapes) => Some((found.def.key, shapes)),
                    _ => None,
                }
            }
        }
    }
}

impl<'q, 'r, 'a> World<&'a str> for ValueWorld<'q, 'r, 'a> {
    type Scope<'w>
        = ValueScope<'w, 'a>
    where
        Self: 'w;
    type Root<'w>
        = ValueRoot<'w, 'a>
    where
        Self: 'w;

    fn scope(&self, module: ItemKey) -> Option<ValueScope<'_, 'a>> {
        if self.own(module) {
            let scope = &self.own.done.get(&module)?.scope;
            return Some(ValueScope::Own { scope, cut: (module == self.module).then_some(self.cut) });
        }
        self.built.of(module)?.scope(module)?;
        Some(ValueScope::Dep { built: self.built, module, alloc: self.alloc })
    }

    fn chain(&self, module: ItemKey) -> Option<&[ItemKey]> {
        if self.own(module) {
            return self.own.chains.get(&module).map(|c| c.as_slice());
        }
        self.built.of(module)?.chain(module)
    }

    fn root_of(&self, key: ItemKey) -> Option<ValueRoot<'_, 'a>> {
        if self.own(key) {
            return Some(ValueRoot::Own(&self.own.crate_root));
        }
        let dep = self.built.of(key)?;
        Some(self.dep_root(dep))
    }

    fn root(&self, krate: CrateItemKey) -> Option<ValueRoot<'_, 'a>> {
        if krate == self.own.key {
            return Some(ValueRoot::Own(&self.own.crate_root));
        }
        let at = *self.built.by_prefix.get(&krate.prefix())?;
        let dep = self.built.crates.get(at).copied().flatten().filter(|d| d.key() == krate)?;
        Some(self.dep_root(dep))
    }

    fn variant(&self, owner: ItemKey, name: &'a str) -> Option<Option<VariantHit>> {
        if self.own(owner) {
            return Some(variant_in(self.own.tables.enums.get(&owner)?.as_slice(), name));
        }
        Some(self.built.of(owner)?.enum_of(owner)?.find(name))
    }

    fn no_implicit_prelude(&self, module: ItemKey) -> bool {
        let Some(chain) = self.own.chains.get(&module) else { return false };
        chain.iter().any(|m| self.own.info(*m).is_some_and(|info| info.no_implicit_prelude))
    }

    fn anonymous(&self, module: ItemKey) -> bool {
        self.own(module) && self.own.anonymous(module)
    }
}

// ---------------------------------------------------------------------------------------------
// The expansion.

/// A call in a stream, by token index: its path `path.0..path.1` (before the `!`), its group
/// `open..=close`; `def_crate`: a single name a `local_inner_macros` body wrote, read as
/// `$crate::name` (blaze-expand `PathScope::DefCrate`).
#[derive(Clone, Copy)]
struct CallAt {
    path: (usize, usize),
    open: usize,
    close: usize,
    def_crate: bool,
}

/// One site's expansion: its world, its host, and what it read so far.
struct Driver<'q, 'r, 'a, C> {
    world: ValueWorld<'q, 'r, 'a>,
    bump: Bump<'a>,
    cx: &'q C,
    /// The crate's edition: the site's tokens' and its calls'.
    edition: Edition,
    item: ItemKey,
    attr: &'static str,
    reads: BVec<'a, ValueRead<'a>>,
    /// The last form reached, rendered.
    form: &'a str,
}

/// `lo..hi` of `stream` without the invisible groups around all of it.
fn peeled(stream: &TokenStream<'_>, mut lo: usize, mut hi: usize) -> (usize, usize) {
    let tokens = stream.tokens();
    while hi >= lo + 2 && tokens[lo].kind == TokenKind::Open(Delim::Invisible) && stream.close_of(lo) == Some(hi - 1) {
        lo += 1;
        hi -= 1;
    }
    (lo, hi)
}

fn is_punct(stream: &TokenStream<'_>, at: usize, c: char) -> bool {
    matches!(stream.tokens().get(at).map(|t| t.kind), Some(TokenKind::Punct { char, .. }) if char == c)
}

fn is_colons(stream: &TokenStream<'_>, at: usize) -> bool {
    is_punct(stream, at, ':') && is_punct(stream, at + 1, ':')
}

/// `$crate` at `at`: a `$` and the word `crate`. Its token count.
fn dollar_crate(stream: &TokenStream<'_>, at: usize) -> Option<usize> {
    if is_punct(stream, at, '$') && at + 1 < stream.len() && stream.text_at(at + 1) == "crate" {
        return Some(2);
    }
    match stream.tokens().get(at).map(|t| t.kind) {
        Some(TokenKind::Ident { .. }) if stream.text_at(at) == "$crate" => Some(1),
        _ => None,
    }
}

/// The stream as ONE function-like call `path!(..)` (any delimiter; the path's segments words
/// or `$crate`, joined by `::`, a leading `::` allowed), or `None`.
fn one_call(stream: &TokenStream<'_>) -> Option<CallAt> {
    let (lo, hi) = peeled(stream, 0, stream.len());
    let tokens = stream.tokens();
    let mut i = lo;
    if is_colons(stream, i) {
        i += 2;
    }
    loop {
        match (dollar_crate(stream, i), tokens.get(i).map(|t| t.kind)) {
            (Some(n), _) => i += n,
            (None, Some(TokenKind::Ident { .. })) => i += 1,
            _ => return None,
        }
        if is_colons(stream, i) {
            i += 2;
        } else {
            break;
        }
    }
    let path = (lo, i);
    if !is_punct(stream, i, '!') {
        return None;
    }
    let open = i + 1;
    match tokens.get(open).map(|t| t.kind) {
        Some(TokenKind::Open(Delim::Paren | Delim::Bracket | Delim::Brace)) => {}
        _ => return None,
    }
    let close = stream.close_of(open)?;
    (close + 1 == hi).then_some(CallAt { path, open, close, def_crate: false })
}

impl<'q, 'r, 'a, C: ExpandCx> Driver<'q, 'r, 'a, C> {
    fn alloc(&self) -> Alloc<'a> {
        self.world.alloc
    }

    fn refused(&self, why: ValueWhy<'a>) -> ValueRefused<'a> {
        ValueRefused { item: self.item, attr: self.attr, why, form: self.form }
    }

    /// The last form reached is `stream`.
    fn reached(&mut self, stream: &TokenStream<'_>) {
        let mut text = AString::new_in(self.alloc());
        write_tokens(stream, &mut text);
        self.form = self.bump.alloc_str(&text);
    }

    /// The site's value.
    fn value(&mut self, site: ValueSite<'a>) -> Result<&'a str, ValueRefused<'a>> {
        let alloc = self.alloc();
        let stream = match site.tokens {
            CallTokens::Text => lex(site.value, self.edition, alloc).map_err(|lex| self.refused(ValueWhy::Tokens { lex }))?,
            CallTokens::Expansion(tokens) => tokens.copy_in(alloc).map_err(|error| self.refused(ValueWhy::Input { error }))?,
            CallTokens::Refused(error) => return Err(self.refused(ValueWhy::Nested(error))),
        };
        let stream = self.reduce(stream, 0)?;
        self.literal(&stream)
    }

    /// `stream` expanded while it is one call: the call's expansion, and its expansion's, at
    /// most [`EAGER_DEPTH`] deep counted from `depth`.
    fn reduce(&mut self, mut stream: TokenStream<'a>, mut depth: usize) -> Result<TokenStream<'a>, ValueRefused<'a>> {
        while let Some(at) = one_call(&stream) {
            stream = self.expand_one(&stream, at, depth)?;
            depth += 1;
        }
        Ok(stream)
    }

    /// The value of the last form: one string literal, no suffix, its escapes read.
    fn literal(&mut self, stream: &TokenStream<'a>) -> Result<&'a str, ValueRefused<'a>> {
        self.reached(stream);
        let (lo, hi) = peeled(stream, 0, stream.len());
        if hi != lo + 1 {
            return Err(self.refused(ValueWhy::NotAString));
        }
        let token = stream.tokens()[lo];
        match token.kind {
            TokenKind::Literal { kind: LitKind::Str | LitKind::StrRaw(_), suffix_len: 0, .. } => {
                let mut text = AString::new_in(self.alloc());
                write_token(stream, &token, &mut text);
                attr_literal(&text, self.edition, self.bump).map_err(|_| self.refused(ValueWhy::Escape))
            }
            TokenKind::Literal { kind: LitKind::Str | LitKind::StrRaw(_), .. } => Err(self.refused(ValueWhy::Suffixed)),
            _ => Err(self.refused(ValueWhy::NotAString)),
        }
    }

    /// The path of the call at `at` as segments in `'a` (`$crate` as one), whether it starts
    /// with `::`, and its first token's mark (what a `$crate` names).
    fn segments(&self, stream: &TokenStream<'_>, at: CallAt) -> (AVec<'a, &'a str>, bool, Option<TextMark>) {
        let alloc = self.alloc();
        let (start, end) = at.path;
        let mut segments: AVec<'a, &'a str> = AVec::new_in(alloc);
        if at.def_crate {
            segments.push("$crate");
        }
        let abs = is_colons(stream, start);
        let first = stream.tokens().get(start).and_then(|t| stream.hygiene(t.ctx).mark).map(TextMark::of);
        let mut i = start;
        while i < end {
            if let Some(n) = dollar_crate(stream, i) {
                segments.push("$crate");
                i += n;
                continue;
            }
            if let Some(TokenKind::Ident { .. }) = stream.tokens().get(i).map(|t| t.kind) {
                segments.push(keep(stream.text_at(i), self.bump));
            }
            i += 1;
        }
        (segments, abs, first)
    }

    /// The call at `at` of `stream`, expanded at the site: its path walked in the macro
    /// namespace, its definition built, its input's calls answered first when it asks
    /// (`concat!`), its result sealed by its call's key.
    fn expand_one(&mut self, stream: &TokenStream<'a>, at: CallAt, depth: usize) -> Result<TokenStream<'a>, ValueRefused<'a>> {
        struct Frame<'a> {
            mac: &'a str,
            definition: blaze_expand::MacroDefinition<'a>,
            args: TokenStream<'a>,
            site: CallSite,
            depth: usize,
            nested: AVec<'a, blaze_expand::NestedCall>,
            next_idx: usize,
            answers: AVec<'a, TokenStream<'a>>,
            reduce_depth: usize,
        }
        let alloc = self.alloc();
        let mut stack: AVec<'a, Frame<'a>> = AVec::new_in(alloc);
        let mut cur_owned: Option<TokenStream<'a>> = None;
        let mut cur_at = at;
        let mut cur_depth = depth;
        loop {
            let cur_stream = cur_owned.as_ref().unwrap_or(stream);
            self.reached(cur_stream);
            let (segments, abs, first) = self.segments(cur_stream, cur_at);
            let mac: &'a str = segments.last().copied().unwrap_or("");
            if cur_depth >= EAGER_DEPTH {
                return Err(self.refused(ValueWhy::TooDeep { mac }));
            }
            let own = self.world.own;
            let (kind, skip) = to_path_head(abs, &segments, first, own.key);
            let written = Written { kind, segments: &segments[skip..] };
            let module = self.world.module;
            let resolved = crate::names::path(&self.world, module, Namespace::Macros, written, alloc).map_err(|why| self.refused(ValueWhy::NotFound { mac, why }))?;
            let Some(def) = resolved.whole(written.segments.len()) else {
                let why = PathError { module, at: resolved.consumed, why: Why::NotAModule };
                return Err(self.refused(ValueWhy::NotFound { mac, why }));
            };
            // The source, from the crate's tables or the dependency that defines it (asked).
            let (src, krate, runner, edition) = match own.tables.macros.get(&def.key) {
                Some(src) => (Some(*src), own.key, own.proc_key, own.edition),
                None => match self.world.built.of(def.key) {
                    Some(dep) => {
                        let src = dep.macro_of(def.key);
                        self.world.built.ask(|| Question::macro_src(def.key, src, alloc));
                        (src, dep.key(), dep.proc_key(), dep.edition())
                    }
                    None => (None, own.key, None, own.edition),
                },
            };
            let Some(src) = src else { return Err(self.refused(ValueWhy::NoSource { mac })) };
            self.read(ValueRead::Macro { def: def.key, digest: crate::asked::macro_digest(Some(src)) });
            let definition = match definition_of(src, krate, runner, edition, CallShape::FnLike, alloc) {
                Ok(definition) => definition,
                Err(note) => {
                    let mut text = BString::new_in(self.bump);
                    let _ = write!(text, "{note:?}");
                    let note: &'a str = text.into_bump_str();
                    return Err(self.refused(ValueWhy::Definition { mac, note }));
                }
            };
            let args = cur_stream.copy_in(cur_at.open + 1..cur_at.close, alloc).map_err(|error| self.refused(ValueWhy::Stream { mac, error }))?;
            let site = CallSite { edition: self.edition, panic_edition: self.edition, ctx: Ctx::root(self.edition) };
            let call = Call { kind: CallKind::FnLike { args: &args }, site, eager: Eager { inside: Inside::EagerInput, answers: &[] } };
            let failed = |error| ValueWhy::Expand { mac, error };
            let (mut expansion, mut key, mut finished_mac) = match expand(&definition, &call, self.cx, alloc).map_err(|error| self.refused(failed(error)))? {
                Expanded::Expansion(expansion) => (expansion, call_key(&definition, &call).0, mac),
                Expanded::NeedsExpanded(nested) => {
                    // Each call in the input, expanded at the site in written order (and its
                    // result while it is one call), then the builtin entered again with them.
                    if let Some(first_inner) = nested.first() {
                        if first_inner.kind != NestedKind::FnLike {
                            return Err(self.refused(ValueWhy::NotACall { mac }));
                        }
                        let (start, end) = (first_inner.path.start as usize, first_inner.args.end as usize);
                        let one = args.copy_in(start..end, alloc).map_err(|error| self.refused(ValueWhy::Stream { mac, error }))?;
                        let next_at = CallAt {
                            path: (0, first_inner.path.end as usize - start),
                            open: first_inner.args.start as usize - start,
                            close: end - 1 - start,
                            def_crate: first_inner.scope == PathScope::DefCrate,
                        };
                        let answers = AVec::with_capacity_in(nested.len(), alloc);
                        stack.push(Frame {
                            mac,
                            definition,
                            args,
                            site,
                            depth: cur_depth,
                            nested,
                            next_idx: 0,
                            answers,
                            reduce_depth: cur_depth + 2,
                        });
                        cur_owned = Some(one);
                        cur_at = next_at;
                        cur_depth += 1;
                        continue;
                    }
                    let again = Call { kind: CallKind::FnLike { args: &args }, site, eager: Eager { inside: Inside::EagerInput, answers: &[] } };
                    match expand(&definition, &again, self.cx, alloc).map_err(|error| self.refused(failed(error)))? {
                        Expanded::Expansion(expansion) => (expansion, call_key(&definition, &again).0, mac),
                        Expanded::NeedsExpanded(_) => return Err(self.refused(ValueWhy::AskedAgain { mac })),
                    }
                }
            };
            loop {
                if let Some(failed) = Failed::of(&expansion, self.bump) {
                    return Err(self.refused(ValueWhy::Failed { mac: finished_mac, failed }));
                }
                for read in expansion.reads.list() {
                    let subject = keep(expansion.reads.subject(read), self.bump);
                    self.read(ValueRead::Host { kind: read.kind, subject, answer: read.answer });
                }
                let mut tokens = expansion.tokens;
                tokens.seal(key);
                let Some(top) = stack.last_mut() else {
                    return Ok(tokens);
                };
                if let Some(next_at) = one_call(&tokens) {
                    cur_owned = Some(tokens);
                    cur_at = next_at;
                    cur_depth = top.reduce_depth;
                    top.reduce_depth += 1;
                    break;
                }
                top.answers.push(tokens);
                top.next_idx += 1;
                if let Some(inner) = top.nested.get(top.next_idx) {
                    if inner.kind != NestedKind::FnLike {
                        let mac = top.mac;
                        return Err(self.refused(ValueWhy::NotACall { mac }));
                    }
                    let start = usize::try_from(inner.path.start).unwrap_or(0);
                    let end = usize::try_from(inner.args.end).unwrap_or(0);
                    let path_end = usize::try_from(inner.path.end).unwrap_or(0);
                    let args_start = usize::try_from(inner.args.start).unwrap_or(0);
                    let mac = top.mac;
                    let one = top.args.copy_in(start..end, alloc).map_err(|error| self.refused(ValueWhy::Stream { mac, error }))?;
                    cur_owned = Some(one);
                    cur_at = CallAt {
                        path: (0, path_end - start),
                        open: args_start - start,
                        close: end - 1 - start,
                        def_crate: inner.scope == PathScope::DefCrate,
                    };
                    cur_depth = top.depth + 1;
                    top.reduce_depth = top.depth + 2;
                    break;
                }
                let Some(frame) = stack.pop() else {
                    return Err(self.refused(ValueWhy::AskedAgain { mac: finished_mac }));
                };
                let mut refs: AVec<'_, &TokenStream<'_>> = AVec::with_capacity_in(frame.answers.len(), alloc);
                refs.extend(frame.answers.iter());
                let again = Call { kind: CallKind::FnLike { args: &frame.args }, site: frame.site, eager: Eager { inside: Inside::EagerInput, answers: &refs } };
                let mac = frame.mac;
                let failed = |error| ValueWhy::Expand { mac, error };
                let (exp, k) = match expand(&frame.definition, &again, self.cx, alloc).map_err(|error| self.refused(failed(error)))? {
                    Expanded::Expansion(expansion) => (expansion, call_key(&frame.definition, &again).0),
                    Expanded::NeedsExpanded(_) => return Err(self.refused(ValueWhy::AskedAgain { mac })),
                };
                expansion = exp;
                key = k;
                finished_mac = mac;
            }
        }
    }

    /// `read` noted once, in the order first read.
    fn read(&mut self, read: ValueRead<'a>) {
        if !self.reads.contains(&read) {
            self.reads.push(read);
        }
    }
}

#[cfg(test)]
mod tests {
    use blaze_expand::seen::SeenRoot;
    use blaze_unit_graph::{Implicit, Prelude, StdIs};

    use super::*;
    use crate::crate_env::{CrateEnv, EnvFacts};
    use crate::graph::Sysroot;
    use crate::resolve::{CrateInput, SourceTexts, resolve_crates};
    use crate::task::Serial;

    const ROOT: &str = "/ring/src/lib.rs";

    /// One crate, `ring`, its root `text`, its package at `version`.
    struct OneCrate {
        text: &'static str,
        env: CrateEnv<'static>,
    }

    impl SourceTexts<'static> for OneCrate {
        fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
            (krate == 0 && path == ROOT).then_some(self.text)
        }

        fn crate_env(&self, krate: usize) -> Option<&CrateEnv<'static>> {
            (krate == 0).then_some(&self.env)
        }
    }

    /// `text` solved as crate `ring` of package version `version`.
    fn solved(text: &'static str, version: &'static str) -> &'static CrateNames<'static> {
        let bump = crate::tests::SyncFrame::leaked();
        let alloc: Alloc<'static> = bump.alloc();
        let facts = EnvFacts {
            crate_name: "ring",
            package: Some(("ring", version)),
            bin: None,
            seen: SeenRoot::Package,
            read_folder: "/ring/",
            build_script: false,
            script: None,
            test_or_bench: false,
            bin_exes: crate::crate_env::BinNames::NONE,
            meta: None,
        };
        let sources = OneCrate { text, env: CrateEnv::of(facts, alloc).unwrap_or_else(|e| panic!("the crate's environment: {e:?}")) };
        let input = CrateInput {
            name: "ring",
            key: CrateItemKey(ContentId::of("blaze-scope attr_values test crate", b"ring")),
            edition: Edition::E2021,
            implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false },
            root: ROOT,
            shown_from: "/ring/",
            features: &[],
            deps: &[],
            cfg: &[],
            script: None,
        };
        let names = resolve_crates(&[input], Sysroot::default(), &sources, &Serial, bump).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
        names.crates()[0]
    }

    /// The key of the item named `name` among the crate's item texts.
    fn key_named(c: &CrateNames<'_>, name: &str) -> ItemKey {
        c.tables.texts.values().find(|(t, _)| t.name == Some(name)).map(|(t, _)| t.key).unwrap_or_else(|| panic!("no item `{name}`"))
    }

    /// The row of `link_name` on the item named `name`.
    fn link_name<'a>(c: &'a CrateNames<'a>, name: &str) -> ValueRow<'a> {
        *row(&c.values, key_named(c, name), "link_name").unwrap_or_else(|| panic!("no `link_name` row of `{name}`: {:?}", c.values.as_slice()))
    }

    const BUILTINS: &str = "#![no_core]\n\
        #[rustc_builtin_macro]\nmacro_rules! concat { () => {} }\n\
        #[rustc_builtin_macro]\nmacro_rules! stringify { () => {} }\n\
        #[rustc_builtin_macro]\nmacro_rules! env { () => {} }\n";

    /// ring 0.17.14 src/prefixed.rs, its four macros as written, and one `prefixed_extern!`
    /// call (src/limb.rs declares `LIMBS_are_zero` so).
    const RING: &str = "#![no_core]\n\
        #[rustc_builtin_macro]\nmacro_rules! concat { () => {} }\n\
        #[rustc_builtin_macro]\nmacro_rules! stringify { () => {} }\n\
        #[rustc_builtin_macro]\nmacro_rules! env { () => {} }\n\
        macro_rules! core_name_and_version {\n\
            () => {\n\
                concat!(\n\
                    env!(\"CARGO_PKG_NAME\"),\n\
                    \"_core_\",\n\
                    env!(\"CARGO_PKG_VERSION_MAJOR\"),\n\
                    \"_\",\n\
                    env!(\"CARGO_PKG_VERSION_MINOR\"),\n\
                    \"_\",\n\
                    env!(\"CARGO_PKG_VERSION_PATCH\"),\n\
                    \"_\",\n\
                    env!(\"CARGO_PKG_VERSION_PRE\"), // Often empty\n\
                )\n\
            };\n\
        }\n\
        macro_rules! prefix {\n\
            ( ) => {\n\
                concat!(core_name_and_version!(), \"_\")\n\
            };\n\
        }\n\
        macro_rules! prefixed_extern {\n\
            {\n\
                $(\n\
                    $( #[$meta:meta] )*\n\
                    $vis:vis fn $name:ident ( $( $arg_pat:ident : $arg_ty:ty ),* $(,)? )\n\
                    $( -> $ret_ty:ty )?;\n\
                )+\n\
            } => {\n\
                extern \"C\" {\n\
                    $(\n\
                        prefixed_item! {\n\
                            link_name\n\
                            $name\n\
                            {\n\
                                $( #[$meta] )*\n\
                                $vis fn $name ( $( $arg_pat : $arg_ty ),* ) $( -> $ret_ty )?;\n\
                            }\n\
                        }\n\
                    )+\n\
                }\n\
            };\n\
        }\n\
        macro_rules! prefixed_item {\n\
            {\n\
                $attr:ident\n\
                $name:ident\n\
                { $item:item }\n\
            } => {\n\
                #[$attr = concat!(prefix!(), stringify!($name))]\n\
                $item\n\
            };\n\
        }\n\
        prefixed_extern! {\n\
            fn LIMBS_are_zero(num_limbs: usize) -> usize;\n\
        }\n";

    /// The variable `name`'s read among `reads`.
    fn env_read<'a>(reads: &[ValueRead<'a>], name: &str) -> Option<ValueRead<'a>> {
        reads.iter().copied().find(|r| matches!(r, ValueRead::Host { kind: ReadKind::Env, subject, .. } if *subject == name))
    }

    /// (a) ring's exact shape: `prefixed_extern!` writes an extern block of `prefixed_item!`
    /// calls, each writing `#[link_name = concat!(prefix!(), stringify!(f))] fn f(..);`, its
    /// value expanded at the item: `prefix!()` (a crate-local `macro_rules!`, found in the
    /// textual scope) gives `concat!(core_name_and_version!(), "_")`, whose `env!`s read the
    /// package (`CARGO_PKG_VERSION_PRE` empty, so the underscore is doubled). (d) CONTROL: the
    /// same crate at major version 1 moves the symbol and the major variable's read.
    ///
    /// Prints `attr_values: ring_core_0_17_14__LIMBS_are_zero; at 1.17.14 ring_core_1_17_14__LIMBS_are_zero`.
    /// Without the piece: the link table's `NotAString(NeedsContext { mac: "prefix" })`.
    #[test]
    fn ring_s_prefixed_link_name_is_its_symbol() {
        let ring = solved(RING, "0.17.14");
        let row = link_name(ring, "LIMBS_are_zero");
        assert_eq!(row.value, Ok("ring_core_0_17_14__LIMBS_are_zero"), "ring's symbol: {:?}", row.value.map_err(|e| std::format!("{e}")));
        assert_eq!(ring.values.len(), 1, "one value site in the crate: {:?}", ring.values.as_slice());
        for name in ["CARGO_PKG_NAME", "CARGO_PKG_VERSION_MAJOR", "CARGO_PKG_VERSION_MINOR", "CARGO_PKG_VERSION_PATCH", "CARGO_PKG_VERSION_PRE"] {
            assert!(env_read(row.reads, name).is_some(), "the value read `{name}`: {:?}", row.reads);
        }
        assert!(row.reads.iter().any(|r| matches!(r, ValueRead::Macro { def, .. } if *def == key_named_macro(ring, "prefix"))), "the value read `prefix!`'s source: {:?}", row.reads);

        // CONTROL (d): the major version moved.
        let one = solved(RING, "1.17.14");
        let moved = link_name(one, "LIMBS_are_zero");
        assert_eq!(moved.value, Ok("ring_core_1_17_14__LIMBS_are_zero"));
        assert_ne!(env_read(row.reads, "CARGO_PKG_VERSION_MAJOR"), env_read(moved.reads, "CARGO_PKG_VERSION_MAJOR"), "the major's read moved with it");
        assert_eq!(env_read(row.reads, "CARGO_PKG_VERSION_MINOR"), env_read(moved.reads, "CARGO_PKG_VERSION_MINOR"), "the minor's did not");
        std::println!("attr_values: {}; at 1.17.14 {}", row.value.unwrap_or(""), moved.value.unwrap_or(""));
    }

    /// The key of the `macro_rules!` named `name` among the crate's macros.
    fn key_named_macro(c: &CrateNames<'_>, name: &str) -> ItemKey {
        c.tables.macros.iter().find(|(_, m)| m.name == name).map(|(k, _)| *k).unwrap_or_else(|| panic!("no macro `{name}`"))
    }

    /// (b) The Reference, "Macros By Example", "Textual scope": a `macro_rules!` is in scope
    /// after its definition. TWIN: `prefix!` written BELOW the item, with no `#[macro_export]`
    /// or `#[macro_use]`, is "cannot find macro" at the item, naming the item and the
    /// attribute; the same definition written above it gives the value.
    ///
    /// Prints `attr_values: below the item: #[link_name = ..] on item ..: cannot find macro `prefix` in this scope ..`.
    #[test]
    fn a_macro_written_after_the_item_is_not_in_its_scope() {
        fn text(above: bool) -> &'static str {
            let def = "macro_rules! prefix { () => { \"p_\" } }\n";
            let item = "extern \"C\" {\n    #[link_name = concat!(prefix!(), \"x\")]\n    fn x();\n}\n";
            let (first, second) = if above { (def, item) } else { (item, def) };
            std::boxed::Box::leak(std::format!("{BUILTINS}{first}{second}").into_boxed_str())
        }
        let below = solved(text(false), "0.17.14");
        let row = link_name(below, "x");
        let refused = row.value.expect_err("a macro defined after the item is not in its textual scope");
        assert!(matches!(refused.why, ValueWhy::NotFound { mac: "prefix", why: PathError { why: Why::NotFound, .. } }), "{refused}");
        assert_eq!((refused.item, refused.attr), (key_named(below, "x"), "link_name"), "the refusal names the item and the attribute");
        let shown = std::format!("{refused}");
        assert!(shown.contains("cannot find macro `prefix`") && shown.contains("link_name"), "{shown}");

        let above = solved(text(true), "0.17.14");
        assert_eq!(link_name(above, "x").value, Ok("p_x"), "twin: written above, it is in scope");
        std::println!("attr_values: below the item: {shown}");
    }

    /// (c) `concat!()` gives the empty string (core/src/macros/mod.rs: "concatenates literals
    /// into a static string slice"); `stringify!` gives its tokens' text; a macro expanding to
    /// `5` is no string literal and is refused (the Reference: "must macro-expand to literal
    /// expressions" is the grammar; `link_name`'s is MetaNameValueStr, a string).
    #[test]
    fn concat_stringify_and_a_number() {
        let text = "#![no_core]\n\
            #[rustc_builtin_macro]\nmacro_rules! concat { () => {} }\n\
            #[rustc_builtin_macro]\nmacro_rules! stringify { () => {} }\n\
            macro_rules! five { () => { 5 } }\n\
            extern \"C\" {\n\
                #[link_name = concat!()] fn a();\n\
                #[link_name = stringify!(b_sym)] fn b();\n\
                #[link_name = five!()] fn c();\n\
            }\n";
        let c = solved(text, "0.17.14");
        assert_eq!(link_name(c, "a").value, Ok(""));
        assert_eq!(link_name(c, "b").value, Ok("b_sym"));
        let number = link_name(c, "c").value.expect_err("a number is no string");
        assert_eq!(number.why, ValueWhy::NotAString);
        assert_eq!(number.form, "5", "the last form reached is the number");
    }

    /// (e) A crate with no value written as a call: no site is walked, no row is made.
    #[test]
    fn a_crate_with_no_call_value_has_no_rows() {
        let c = solved("#![no_core]\npub fn f() {}\nextern \"C\" { #[link_name = \"plain\"] fn g(); }\n", "0.17.14");
        assert!(c.tables.values.is_empty(), "no site: {:?}", c.tables.values.as_slice());
        assert!(c.values.is_empty(), "no row: {:?}", c.values.as_slice());
    }

    /// The items record of `c`, written and opened in place: the bytes its `CrateScope` record
    /// files, and the record a reader over records opens of them.
    fn items_record_of(c: &'static CrateNames<'static>) -> (&'static [u8], crate::items_record::ItemsRecord<'static>) {
        let bump = crate::tests::SyncFrame::leaked();
        let alloc: Alloc<'static> = bump.alloc();
        let written = crate::items_record::encode(c, alloc).unwrap_or_else(|e| panic!("the items record was not written: {e:?}"));
        let bytes = crate::resolve::aligned(&written, bump);
        let record = crate::items_record::ItemsRecord::open(bytes, alloc).unwrap_or_else(|e| panic!("the items record does not open: {e:?}"));
        (bytes, record)
    }

    /// The header field `i` of an items record's bytes.
    fn header_field(bytes: &[u8], i: usize) -> u32 {
        let at = 8 + 4 * i;
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    }

    /// (g) What outlives the solve is a record (rule 51): ring's value row, written into the
    /// crate's items record and read back from it (`ItemsRecord::attr_value`, what front2's
    /// `Records::attr_value` answers with, one call), is the solve's answer: the same string;
    /// a refused row is its refusal's words; an item or an attribute with no row has none.
    ///
    /// Prints `attr_values: the record reads back ring_core_0_17_14__LIMBS_are_zero and a refusal's words`.
    #[test]
    fn a_value_row_reads_back_from_the_items_record() {
        let ring = solved(RING, "0.17.14");
        let key = key_named(ring, "LIMBS_are_zero");
        let (_, record) = items_record_of(ring);
        let solved_value = link_name(ring, "LIMBS_are_zero").value.ok();
        assert_eq!(solved_value, Some("ring_core_0_17_14__LIMBS_are_zero"));
        assert_eq!(record.attr_value(key, "link_name").ok(), Some(solved_value.map(Ok)), "the record's answer is the solve's");
        assert_eq!(record.attr_value(key, "export_name").ok(), Some(None), "no row of another attribute");
        assert_eq!(record.attr_value(key, "no_mangle").ok(), Some(None), "no table of an attribute that takes no call");
        assert_eq!(record.attr_value(ItemKey(ContentId([7; 32])), "link_name").ok(), Some(None), "no row of another item");

        // A refused row reads back as the solve's words.
        let text = std::boxed::Box::leak(
            std::format!("{BUILTINS}extern \"C\" {{\n    #[link_name = concat!(prefix!(), \"x\")]\n    fn x();\n}}\nmacro_rules! prefix {{ () => {{ \"p_\" }} }}\n").into_boxed_str(),
        );
        let below = solved(text, "0.17.14");
        let refused = link_name(below, "x").value.expect_err("a macro written after the item is not in its scope");
        let words = std::format!("{refused}");
        let (_, record) = items_record_of(below);
        assert_eq!(record.attr_value(key_named(below, "x"), "link_name").ok(), Some(Some(Err(words.as_str()))), "the refusal's words");
        std::println!("attr_values: the record reads back {} and a refusal's words", solved_value.unwrap_or(""));
    }

    /// (h) The `Item` cell of an item holds its value rows (names_cells.rs `item_digest`, rule
    /// 21): ring at 0.17.14 and at 1.17.14 is one text, so `LIMBS_are_zero` has one key and one
    /// text in both, and only its `link_name` value moved (`CARGO_PKG_VERSION_MAJOR`): its cell
    /// moves. CONTROL: `plain`, an item with no row, holds the same cell in both.
    #[test]
    fn an_item_cell_moves_with_its_value_alone() {
        fn item_cell(c: &'static CrateNames<'static>, key: ItemKey) -> Option<ContentId> {
            let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
            let erased = blaze_own::Erased::new(&frame);
            let bump = erased.bump();
            let alloc = bump.alloc();
            let names = crate::names_record::encode(c, alloc).unwrap_or_else(|e| panic!("the names record was not written: {e:?}"));
            let items = crate::items_record::encode(c, alloc).unwrap_or_else(|e| panic!("the items record was not written: {e:?}"));
            let table = crate::names_cells::crate_table(c, &names, &items, &[], bump).unwrap_or_else(|e| panic!("the cell table: {e:?}"));
            crate::names_cells::seen_in(&table, crate::names_cells::NameCell::Item { key }.key()).unwrap_or_else(|e| panic!("the `Item` cell: {e:?}"))
        }
        let text: &'static str = std::boxed::Box::leak(std::format!("{RING}pub fn plain() {{}}\n").into_boxed_str());
        let (zero, one) = (solved(text, "0.17.14"), solved(text, "1.17.14"));
        let (f, f_one) = (key_named(zero, "LIMBS_are_zero"), key_named(one, "LIMBS_are_zero"));
        let (p, p_one) = (key_named(zero, "plain"), key_named(one, "plain"));
        assert_eq!((f, p), (f_one, p_one), "an item's key holds no version");
        assert_ne!(link_name(zero, "LIMBS_are_zero").value, link_name(one, "LIMBS_are_zero").value, "the value moved");
        let (was, now) = (item_cell(zero, f), item_cell(one, f));
        assert!(was.is_some() && now.is_some(), "`LIMBS_are_zero` has an `Item` cell");
        assert_ne!(was, now, "the value moved and the text did not: the cell moved");
        let plain = item_cell(zero, p);
        assert!(plain.is_some(), "`plain` has an `Item` cell");
        assert_eq!(plain, item_cell(one, p), "an item with no row keeps its cell");
    }

    /// (i) A crate with no value written as a call writes no value section: both header fields
    /// stay 0 (the record is the one it was before the table existed) and every read is "no
    /// row".
    #[test]
    fn a_crate_with_no_rows_writes_no_section() {
        let c = solved("#![no_core]\npub fn f() {}\nextern \"C\" { #[link_name = \"plain\"] fn g(); }\n", "0.17.14");
        assert!(c.values.is_empty());
        let (bytes, record) = items_record_of(c);
        use crate::items_record::values::{F_VALUES_AT, F_VALUES_COUNT};
        assert_eq!((header_field(bytes, F_VALUES_COUNT), header_field(bytes, F_VALUES_AT)), (0, 0), "no section");
        assert_eq!(record.attr_value(key_named(c, "g"), "link_name").ok(), Some(None), "no row");
    }

    /// (f) A macro that expands to a call of itself is refused at [`EAGER_DEPTH`] by name,
    /// never followed further (rule 18).
    #[test]
    fn a_self_expanding_value_is_refused_at_the_depth() {
        let text = "#![no_core]\n\
            macro_rules! again { () => { again!() } }\n\
            extern \"C\" { #[link_name = again!()] fn d(); }\n";
        let c = solved(text, "0.17.14");
        let refused = link_name(c, "d").value.expect_err("a value that never ends is refused");
        assert_eq!(refused.why, ValueWhy::TooDeep { mac: "again" });
        assert!(std::format!("{refused}").contains("`again!`"));
    }
}
