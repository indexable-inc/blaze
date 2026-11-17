//! Items declared inside a body: a fn, const, static, type, impl or `use` written in a block,
//! by hand or by a macro's expansion (core's `assert_unsafe_precondition!` writes
//! `const fn precondition_check(..)` inside a block and calls it).
//!
//! The Reference, "Statements", "Item declarations": "Declaring an item within a statement
//! block restricts its scope to the block containing the statement. The item is not given a
//! canonical path nor are any sub-items it may declare." "There is no implicit capture of the
//! containing function's generic parameters, parameters, and local variables." "Names",
//! "Scopes", item scopes: "The name of an item declared directly in a block is in scope for the
//! entire block", before its declaration too, and shadows the outer names of its namespace.
//!
//! What is here (docs/v2/plan/design/typeck.md, "Items inside bodies"; decision D62):
//! - THE TEXT. A body's lowering is dropped with its `Body`; a local item's head borrows its
//!   text for `'n` like any item's. So when a body that holds local items is lowered, its rows
//!   are KEPT in the command's frame (made by the caller of `Front2::new`): a row's text is a
//!   range of the enclosing fn's own text where it is written there, and a copy into the frame
//!   where an expansion wrote it. A body with no local item keeps nothing.
//! - IDENTITY. A local item's library key is the names library's derivation
//!   (`blaze_scope::task::key_of`) from its ENCLOSING body's key, its kind's tag, its name and
//!   its ordinal among the rows of the same kind and name of that body, with
//!   [`ItemKey::INNER`](blaze_id::ItemKey::INNER) set. Rows are in the lowering's order, a
//!   function of the body's text alone: no key depends on which body was checked first, and an
//!   edit of the body re-keys its local items and nothing else.
//! - LOOKUP. [`Locals::find`]: the block's items, then each enclosing block's, innermost
//!   first; a local item's own body continues in the blocks around its declaration (its
//!   enclosing body's), never in that body's generics or locals; nothing found: the module.

use blaze_hash::HashMap;
use blaze_id::ItemKey as LibKey;
use blaze_lower::{Range, RowIdx, ScopeId, View};
use blaze_mem::Frame;
use blaze_own::Alloc;
use blaze_scope::crate_roots::TextMarks;
use super::source::NamesSource;
use blaze_surface::{ItemDetail, Rename, UseForm};
use blaze_typeck::ItemKey;
use parser::SyntaxKind;

use super::foreign_locals::{ForeignKind, LocalForeign};
use super::heads::HeadError;
use super::keys::{Keys, checker_of};
use super::types::Why;

/// The kind of a local item, from its syntax node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalKind {
    Fn,
    Const,
    Static,
    Struct,
    Enum,
    Union,
    Trait,
    TypeAlias,
    Impl,
    Use,
    Mod,
    ExternBlock,
    ExternCrate,
    MacroRules,
    MacroDef,
    /// A node the lowering called an item that none of the above is.
    Other,
}

impl LocalKind {
    fn of(syntax: u16) -> LocalKind {
        match blaze_file_items::syntax_kind(syntax) {
            SyntaxKind::FN => LocalKind::Fn,
            SyntaxKind::CONST => LocalKind::Const,
            SyntaxKind::STATIC => LocalKind::Static,
            SyntaxKind::STRUCT => LocalKind::Struct,
            SyntaxKind::ENUM => LocalKind::Enum,
            SyntaxKind::UNION => LocalKind::Union,
            SyntaxKind::TRAIT => LocalKind::Trait,
            SyntaxKind::TYPE_ALIAS => LocalKind::TypeAlias,
            SyntaxKind::IMPL => LocalKind::Impl,
            SyntaxKind::USE => LocalKind::Use,
            SyntaxKind::MODULE => LocalKind::Mod,
            SyntaxKind::EXTERN_BLOCK => LocalKind::ExternBlock,
            SyntaxKind::EXTERN_CRATE => LocalKind::ExternCrate,
            SyntaxKind::MACRO_RULES => LocalKind::MacroRules,
            SyntaxKind::MACRO_DEF => LocalKind::MacroDef,
            _ => LocalKind::Other,
        }
    }

    /// The tag a key is derived with: the names library's (blaze-scope task/lower.rs
    /// `kind_tag`), so a local item's key is made as a module item's is.
    pub(super) fn tag(self) -> &'static str {
        match self {
            LocalKind::Fn => "Function",
            LocalKind::Const => "Const",
            LocalKind::Static => "Static",
            LocalKind::Struct => "Struct",
            LocalKind::Enum => "Enum",
            LocalKind::Union => "Union",
            LocalKind::Trait => "Trait",
            LocalKind::TypeAlias => "TypeAlias",
            LocalKind::Impl => "Impl",
            LocalKind::Use => "Use",
            LocalKind::Mod => "Mod",
            LocalKind::ExternBlock => "ExternBlock",
            LocalKind::ExternCrate => "ExternCrate",
            LocalKind::MacroRules => "MacroRules",
            LocalKind::MacroDef => "MacroDef",
            LocalKind::Other => "Other",
        }
    }

    /// Its plain Rust name, for a gap or a refusal.
    pub fn form(self) -> &'static str {
        match self {
            LocalKind::Fn => "a fn in a block",
            LocalKind::Const => "a const in a block",
            LocalKind::Static => "a static in a block",
            LocalKind::Struct => "a struct in a block",
            LocalKind::Enum => "an enum in a block",
            LocalKind::Union => "a union in a block",
            LocalKind::Trait => "a trait in a block",
            LocalKind::TypeAlias => "a type alias in a block",
            LocalKind::Impl => "an impl in a block",
            LocalKind::Use => "a use declaration in a block",
            LocalKind::Mod => "a module in a block",
            LocalKind::ExternBlock => "an extern block in a block",
            LocalKind::ExternCrate => "an extern crate in a block",
            LocalKind::MacroRules => "a macro_rules! in a block",
            LocalKind::MacroDef => "a macro 2.0 in a block",
            LocalKind::Other => "an item of another kind in a block",
        }
    }

    /// Whether a name of this kind is in the value namespace (the Reference, "Names",
    /// "Namespaces": functions, constants, statics, and a tuple or unit struct's constructor).
    fn in_values(self) -> bool {
        matches!(self, LocalKind::Fn | LocalKind::Const | LocalKind::Static | LocalKind::Struct | LocalKind::Use)
    }

    /// Whether a name of this kind is in the type namespace (types, traits, modules, and the
    /// name an `extern crate` binds: the Reference, "Extern crate declarations": "bound into
    /// the declaring scope as the given identifier in the type namespace").
    fn in_types(self) -> bool {
        matches!(self, LocalKind::Struct | LocalKind::Enum | LocalKind::Union | LocalKind::Trait | LocalKind::TypeAlias | LocalKind::Mod | LocalKind::ExternCrate | LocalKind::Use)
    }

    /// Whether an item of this kind is an item of its block's MODULE in the names library
    /// (blaze-scope blocks.rs; `blaze_file_items::nested::NestedKind::makes_module`): a
    /// struct, enum, union, type alias or trait. Its key is the library's, its head is made by
    /// the ordinary code ([`super::heads`]'s `Store::step`), and a path naming it is resolved
    /// by the one name solver at that module ([`Row::module`]). A trait's methods are also
    /// candidates at method calls in its block and the blocks inside it
    /// ([`super::block_traits`]). A `mod` and an `extern crate` are too: a path whose first
    /// segment names one is read at the block's module, where the solver continues into the
    /// module's items or the crate's root.
    pub fn in_block_module(self) -> bool {
        matches!(self, LocalKind::Struct | LocalKind::Enum | LocalKind::Union | LocalKind::TypeAlias | LocalKind::Trait | LocalKind::Mod | LocalKind::ExternCrate)
    }

    /// The gap's words for a struct, enum, union or alias whose block has no module in the
    /// names library: one a macro call in the body wrote (the scan of the body's text does
    /// not see it), or one in the body of an item that is itself local to a body (that item
    /// has front2's derived key, not the library's: typeck.md, "Impls inside blocks", owed).
    pub fn unplaced(self) -> &'static str {
        match self {
            LocalKind::Struct => "a struct in a block that a macro call wrote or that is in a local item's body",
            LocalKind::Enum => "an enum in a block that a macro call wrote or that is in a local item's body",
            LocalKind::Union => "a union in a block that a macro call wrote or that is in a local item's body",
            LocalKind::TypeAlias => "a type alias in a block that a macro call wrote or that is in a local item's body",
            LocalKind::Trait => "a trait in a block that a macro call wrote or that is in a local item's body",
            LocalKind::Mod => "a mod in a block that a macro call wrote or that is in a local item's body",
            LocalKind::ExternCrate => "an extern crate in a block that a macro call wrote or that is in a local item's body",
            other => other.form(),
        }
    }

    /// The outline entry its head is made from (as the names library's for a module item);
    /// `None` for a kind whose head the store does not make here.
    pub fn detail(self) -> Option<ItemDetail> {
        Some(match self {
            LocalKind::Fn => ItemDetail::Fn { body_holds_items: true },
            LocalKind::Const => ItemDetail::Const { body_holds_items: true },
            LocalKind::Static => ItemDetail::Static { body_holds_items: true },
            _ => return None,
        })
    }
}

/// The namespace a body path's first segment is looked up in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalNs {
    Value,
    Type,
}

/// One local item of a body, as kept.
#[derive(Clone, Copy, Debug)]
pub struct Row<'n> {
    /// The block it is declared in (the body's `ScopeId`).
    pub scope: u32,
    pub kind: LocalKind,
    /// Its name; for a `use`, the name it binds; empty for an `impl`.
    pub name: &'n str,
    pub key: LibKey,
    /// Where it is in the enclosing item, item-relative (an expansion's: its outermost call).
    pub range: Range,
    /// A `use`'s path to what it binds, and whether written with a leading `::`; empty else.
    pub path: &'n [&'n str],
    pub abs: bool,
    /// For a struct, enum, union or type alias ([`LocalKind::in_block_module`]) written in
    /// the body's own text: its block's module in the names library, which binds its name
    /// (checked when kept). A path that names the item is resolved AT this module by the one
    /// name solver, whose answer carries the item's one key (the library's). `None`: the
    /// library made no such module ([`LocalKind::unplaced`]); for every other kind, unused.
    pub module: Option<LibKey>,
}

/// A block of a body that is a module of the names library (blaze-scope blocks.rs): where
/// its braces are in the body's text, and the module's key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockModule {
    /// The offset of its `{` in the body's text.
    pub open: u32,
    /// The offset after its `}`.
    pub close: u32,
    pub module: LibKey,
}

/// A body's local items: each block's parent, and the rows.
#[derive(Clone, Copy, Debug)]
pub struct BodyLocals<'n> {
    pub parents: &'n [Option<u32>],
    pub rows: &'n [Row<'n>],
    /// The body's blocks that are modules holding a kept struct, enum, union or alias (and
    /// the blocks around them), in the order their `{` is written: a type written at an
    /// offset of the body's text is read at the LAST of these that holds the offset (the
    /// innermost), else at the body's module (body.rs `BodyTypes::env_at`).
    pub blocks: &'n [BlockModule],
    /// Each block scope's module, by `ScopeId` (super::scope_table): the one answer to "which
    /// module is this block" for every reader.
    pub scopes: &'n [super::scope_table::ScopeAt],
}

/// What a local item's head is made from: its text, its module (its enclosing body's), the
/// module its head's names are read at, and where it is declared (the body and block whose
/// names its own body sees beyond its own).
#[derive(Clone, Copy, Debug)]
pub struct LocalHead<'n> {
    pub lib: LibKey,
    pub kind: LocalKind,
    pub text: &'n str,
    /// The enclosing body's module: where the item's own body is lowered (its `module_path!()`,
    /// its macro host), its names found first among the blocks around it ([`Locals::find`]).
    pub module: LibKey,
    /// The module of the block it is declared in (super::scope_table `ScopeAt::module`): where
    /// the names its HEAD writes are read. The Reference, "Statements", "Item declarations":
    /// the item's scope is the block, and "Names", "Scopes", "Item scopes": a block's items are
    /// in scope in the whole block, so `unsafe fn do_call<F, R>(data: *mut Data<F, R>)` beside
    /// `union Data<F, R>` in one fn body (the pinned std's `panicking::catch_unwind`) names the
    /// block's `Data`, which the body's module does not hold.
    pub names_at: LibKey,
    pub enclosing: ItemKey,
    pub scope: u32,
    pub range: Range,
    /// The marks of `text`'s words when a macro wrote the enclosing item (front2 item_site.rs):
    /// the enclosing body's own ([`Enclosing::marks`]), of which `text` is a slice. Its head,
    /// its generics and its own body read every path by them, so a `$crate` the expansion
    /// wrote as `crate` names the macro's crate (bstd's `main!` writes `static MANIFEST:
    /// $crate::grant::Manifest` and `fn hosted(cx: &$crate::Cx, ..)` in the user's `main`).
    /// A row a macro call INSIDE the body wrote is a copy: its column is made with it from the
    /// lowered body's marks of the words it copies (`View::marks_in`,
    /// `mark_site::column_of_copy`). `None` for a text of a file, and for a copy with no
    /// marked word.
    pub marks: Option<TextMarks<'n>>,
}

/// A body names a local item of a form whose record this front end does not make yet: the
/// item, its form, and where it is in its enclosing item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalGap {
    pub item: LibKey,
    pub form: &'static str,
    pub range: Range,
}

impl core::fmt::Display for LocalGap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} ({:x}, at {}..{} of its enclosing item) has no record yet", self.form, self.item, self.range.start, self.range.end)
    }
}

/// The body being kept: whose it is and the text it was lowered from.
#[derive(Clone, Copy)]
pub struct Enclosing<'n> {
    /// The body's checker key.
    pub owner: ItemKey,
    /// Its item (a member's: its container).
    pub item: LibKey,
    pub krate: usize,
    pub module: LibKey,
    /// The text the body was lowered from.
    pub text: &'n str,
    /// The marks of `text`'s words when a macro wrote the body's item (`Site::marks` in
    /// body.rs; `text` is a slice of their text); `None` for a text of a file.
    pub marks: Option<TextMarks<'n>>,
}

/// The local items of every body lowered in this command, kept in the command's frame.
pub struct Locals<'n> {
    frame: &'n Frame,
    bodies: HashMap<ItemKey, BodyLocals<'n>, &'n Frame>,
    heads: HashMap<ItemKey, LocalHead<'n>, &'n Frame>,
    /// The fns and statics of `extern` blocks declared in bodies, by checker key: each one's
    /// block's ABI and its safety ([`super::foreign_locals`]). Each is in `heads` too.
    foreign: HashMap<ItemKey, LocalForeign<'n>, &'n Frame>,
    /// The anonymous constants written in a body that name its items ([`super::anon::Ask::in_body`]),
    /// by checker key: the body and the block the expression is written in. A name in the
    /// constant's own body continues there ([`Locals::find`]), as a local item's does; the
    /// constant is no item, so it is not in `heads`.
    anons: HashMap<ItemKey, (ItemKey, u32), &'n Frame>,
    /// The scope table's planted bug, for the test that must catch it; `Planted::None` else.
    planted: super::scope_table::Planted,
}

impl<'n> Locals<'n> {
    pub fn new(frame: &'n Frame) -> Locals<'n> {
        Locals { frame, bodies: HashMap::new_in(frame), heads: HashMap::new_in(frame), foreign: HashMap::new_in(frame), anons: HashMap::new_in(frame), planted: super::scope_table::Planted::None }
    }

    /// Files the anonymous constant `key` as written in block `scope` of the body `owner`
    /// ([`super::anon::file_asked`]). Its key says both ([`super::anon::Ask::in_body`]), so one
    /// key is never filed at two places.
    pub(super) fn file_anon(&mut self, key: ItemKey, owner: ItemKey, scope: u32) {
        self.anons.insert(key, (owner, scope));
    }

    /// Plants the scope table's bug for every body kept after this (tests only:
    /// tests/front2_macro_block_mods.rs must catch it).
    pub fn plant(&mut self, planted: super::scope_table::Planted) {
        self.planted = planted;
    }

    /// The scope table's planted bug (`Planted::None` but in that test).
    pub fn planted(&self) -> super::scope_table::Planted {
        self.planted
    }

    /// Where the names written in block `scope` of the body `owner` are read
    /// (super::scope_table); `None` when the body kept no local items.
    pub fn scope_at(&self, owner: ItemKey, scope: u32) -> Option<super::scope_table::ScopeAt> {
        self.bodies.get(&owner)?.scopes.get(scope as usize).copied()
    }

    /// The local item whose checker key is `key`, when an `extern` block in a body declares
    /// it: its block's ABI as written and whether it is declared `safe`. `None` for every
    /// other item. The code builder asks this of a callee with no body (with the `#[lang]`
    /// and `#[link_name]` of its head's text, [`Locals::head`]).
    pub fn foreign(&self, key: ItemKey) -> Option<LocalForeign<'n>> {
        self.foreign.get(&key).copied()
    }

    /// A copy of `text` kept in the command's frame (an anonymous constant's item text,
    /// [`super::anon`]: a head's text lives as long as the names do).
    pub(super) fn keep_text(&self, text: &str) -> &'n str {
        let frame = self.frame;
        frame.alloc_str(text)
    }

    /// How many bodies kept their local items.
    pub fn kept(&self) -> usize {
        self.bodies.len()
    }

    /// The local item whose checker key is `key`.
    pub fn head(&self, key: ItemKey) -> Option<&LocalHead<'n>> {
        self.heads.get(&key)
    }

    /// The local items of the body `owner`.
    pub fn body(&self, owner: ItemKey) -> Option<&BodyLocals<'n>> {
        self.bodies.get(&owner)
    }

    /// The key every local item of `at`'s body is derived under: the body's own, by its
    /// item and its checker key (a member's body is not its container's).
    fn parent(crate_key: blaze_id::CrateItemKey, at: Enclosing<'_>) -> LibKey {
        super::nested_exports::body_key(crate_key, at.item, at.owner)
    }

    /// Keeps the local items of the body `at` lowered to `view`, once per body: their rows in
    /// the frame, their keys filed in `keys`. A body with none keeps nothing. `alloc` holds
    /// the `use` trees' scratch.
    pub fn keep<S: NamesSource<'n>>(&mut self, names: S, keys: &mut Keys, at: Enclosing<'n>, view: &View<'_>, alloc: Alloc<'_>) -> Result<(), HeadError> {
        if self.bodies.contains_key(&at.owner) {
            return Ok(());
        }
        let bad = |why: Why| HeadError::Item(at.item, why);
        let scopes = view.scopes.len();
        let mut count = 0usize;
        for s in 0..scopes {
            count += view.scope(ScopeId(s as u32)).map_err(|b| bad(Why::from(b)))?.items.len as usize;
        }
        if count == 0 {
            return Ok(());
        }
        // The body's crate's key (every local key is derived under it); a crate the source has
        // not read has no text for the body's item either.
        let crate_key = names.crate_key(at.krate).ok_or(HeadError::NoText(at.item))?;
        let parent = Self::parent(crate_key, at);
        let frame = self.frame;
        // The body's blocks as the names library made them modules: the same scan of the same
        // text, so the same keys (`blaze_scope::blocks::block_modules`). None when the body
        // is not an item or member of the library (a local item's own body).
        let nested = blaze_file_items::nested::scan(at.text, alloc);
        let modules = match Self::library_body(names, at) {
            Some(body) if nested.end == blaze_file_items::nested::NestedEnd::Whole => Some(blaze_scope::blocks::block_modules(crate_key, body, &nested, alloc)),
            _ => None,
        };
        // The module of each block scope of the body, by the scope (super::scope_table): built
        // once, here, and kept with the body. A refusal names the whole item: the table is the
        // body's.
        let whole = Range::new(0, at.text.len() as u32);
        let table = super::scope_table::build(crate_key, at.module, view, &nested, modules.as_deref(), self.planted, alloc)
            .map_err(|r| bad(Why::Unsupported(r.form(), whole)))?;
        // The blocks whose module is known to exist: one that binds a kept item's name, and
        // the blocks around it.
        let mut known: blaze_own::AVec<'_, bool> = blaze_own::AVec::new_in(alloc);
        known.resize(nested.blocks.len(), false);
        let parents: &'n mut [Option<u32>] = frame.alloc_slice_fill_with(scopes, |_| None);
        let mut rows: blaze_own::Vec<Row<'n>, &'n Frame> = blaze_own::Vec::new_in(frame);
        for (s, slot) in parents.iter_mut().enumerate() {
            let scope = view.scope(ScopeId(s as u32)).map_err(|b| bad(Why::from(b)))?;
            *slot = scope.parent.map(|p| p.0);
            // Where a head written in this block reads its names: the block's module (its own
            // items, then the blocks around it through that module's `use super::*`, then the
            // body's module), the scope table's one answer for the scope ([`LocalHead::names_at`]).
            let names_at = table
                .at(s as u32)
                .map(super::scope_table::ScopeAt::module)
                .ok_or_else(|| bad(Why::Unsupported("a block scope of a body that its scope table does not hold", whole)))?;
            for i in scope.items.start..scope.items.start + scope.items.len {
                let row = view.row(RowIdx(i)).map_err(|b| bad(Why::from(b)))?;
                let it = view.local_item(row).map_err(|b| bad(Why::from(b)))?;
                let kind = LocalKind::of(it.syntax);
                let range = view.site(it.range).map_err(|t| bad(Why::from(t)))?;
                // The text: the enclosing item's own where written there, else (an
                // expansion's) a copy into the frame, kept for the command.
                // Its words' marks: the enclosing text's, of which a row written there is a
                // slice ([`LocalHead::marks`]); a copy's, made with it from the lowered body's
                // marks of the expansion words it copies (`View::marks_in`), since no column
                // reaches from the copy back to a marked text.
                let (text, marks): (&'n str, Option<TextMarks<'n>>) = match view.item_relative(it.range) {
                    Some(r) => (at.text.get(r.start as usize..r.end as usize).ok_or_else(|| bad(Why::from(blaze_lower::TextAt::Malformed(it.range))))?, at.marks),
                    None => {
                        let copy: &'n str = frame.alloc_str(view.text_at(it.range).map_err(|t| bad(Why::from(t)))?);
                        let marked = view.marks_in(it.range, alloc).map_err(|t| bad(Why::from(t)))?;
                        let marks = match marked.is_empty() {
                            true => None,
                            false => {
                                let unwritten = |_| bad(Why::Unsupported("a local item's copied text whose marks' column cannot be written", range));
                                let bytes = blaze_lower::mark_site::column_of_copy(copy, &marked, alloc).map_err(unwritten)?;
                                let bytes: &'n [u8] = frame.alloc_slice_copy(&bytes);
                                let (column, _) = blaze_expand::origin_column::OriginsRef::open(bytes).map_err(unwritten)?;
                                Some(TextMarks { text: copy, column })
                            }
                        };
                        (copy, marks)
                    }
                };
                let name: &'n str = match it.name {
                    Some(n) => frame.alloc_str(view.name_text(n).map_err(|t| bad(Why::from(t)))?),
                    None => "",
                };
                // An `extern crate`'s lowering row has no name (its crate is a NAME_REF and
                // its `as` name is inside the rename): the name it binds is read from its
                // text by the Reference's grammar (super::block_crates); `as _` binds none.
                let name: &'n str = if kind == LocalKind::ExternCrate {
                    match super::block_crates::extern_crate_binding(text) {
                        Ok(super::block_crates::ExternBinding::Named(bound)) => bound,
                        Ok(super::block_crates::ExternBinding::Unbound) => "",
                        Err(unread) => return Err(bad(Why::Unsupported(unread.form(), range))),
                    }
                } else {
                    name
                };
                if kind == LocalKind::Use {
                    // A `use` binds one name per leaf; each is a row of its own.
                    let tree = use_tree(text);
                    let Some(leaves) = blaze_scope::task::use_leaves(tree, alloc) else {
                        return Err(bad(Why::Unsupported("a use tree in a block that does not read", range)));
                    };
                    for leaf in leaves.iter() {
                        let bound = match leaf.form {
                            UseForm::Named { rename: Rename::Same, .. } => leaf.segments.last().copied().unwrap_or(""),
                            UseForm::Named { rename: Rename::To(n), .. } => n,
                            // `as _` binds no name, but a trait it names is in scope for
                            // method calls in the block (super::block_traits): its row is kept
                            // under a name no identifier is, never found by `find`.
                            UseForm::Named { rename: Rename::Underscore, .. } => super::block_traits::UNDERSCORE,
                            // A glob's row is kept under a name no identifier is, with the
                            // path before its `*`: a name of the body is looked up through it
                            // lazily, after the block's own items (super::glob_uses).
                            UseForm::Glob => super::glob_uses::GLOB,
                        };
                        let path: &'n [&'n str] = frame.alloc_slice_copy(&leaf.segments);
                        rows.push(Row { scope: s as u32, kind, name: bound, key: parent, range, path, abs: leaf.abs, module: None });
                    }
                    continue;
                }
                if kind == LocalKind::ExternBlock {
                    // An `extern` block declares its fns and statics in the value namespace
                    // of the block it is written in (the Reference, "External blocks",
                    // [items.extern.namespace]): each is a row of this block, a fn's or a
                    // static's like any local one (its key, its text, its head by
                    // `Store::step`), and its block's ABI and its safety are kept beside it
                    // ([`Locals::foreign`]). The block itself binds no name: no row.
                    let edition = names.edition_of(at.krate).ok_or(HeadError::NoText(at.item))?;
                    // Which declarations exist is the crate's cfg's to say (`cfg`, `cfg_attr`).
                    let atoms = names.cfg_atoms(at.krate).ok_or(HeadError::NoText(at.item))?;
                    let (abi, decls) = super::foreign_locals::declared(text, edition, &mut super::heads::CrateCfg(atoms), alloc)
                        .map_err(|r| bad(Why::Unsupported(r.form(), range)))?;
                    let from = view.item_relative(it.range).map(|r| r.start);
                    for d in decls.iter() {
                        let kind = match d.kind {
                            ForeignKind::Fn => LocalKind::Fn,
                            ForeignKind::Static => LocalKind::Static,
                        };
                        // Written in the enclosing item's text: its own place there; an
                        // expansion's: its block's (the outermost call).
                        let range = match from {
                            Some(start) => Range::new(start + d.at.start, start + d.at.end),
                            None => range,
                        };
                        let ordinal = rows.iter().filter(|r| r.kind == kind && r.name == d.name).count() as u32;
                        let key = super::nested_exports::local_key(crate_key, parent, kind.tag(), d.name, ordinal);
                        rows.push(Row { scope: s as u32, kind, name: d.name, key, range, path: &[], abs: false, module: None });
                        let ck = keys.local(key, kind).map_err(HeadError::Key)?;
                        self.heads.insert(ck, LocalHead { lib: key, kind, text: d.text, module: at.module, names_at, enclosing: at.owner, scope: s as u32, range, marks });
                        self.foreign.insert(ck, LocalForeign { kind: d.kind, abi, safe: d.safe });
                    }
                    continue;
                }
                // A struct, enum, union, alias, trait, `mod` or `extern crate` is an item of its
                // block's module (the Reference, "Statements", "Item declarations": its scope is
                // "the block containing the statement", whichever text wrote the block): the
                // module the scope table gives the row's scope as its OWN, when that module binds
                // the name. None: the names library filed no module for the block; a path naming
                // the item is refused by name (`LocalKind::unplaced`), never read at another
                // block.
                let mut in_module = None;
                if kind.in_block_module()
                    && let Some(m) = table.at(s as u32).and_then(|a| a.own())
                    && names.binds(m, blaze_surface::Namespace::Types, name)
                {
                    in_module = Some(m);
                    // The nearest scope at or around the row's that is a block of the body's own
                    // text, and every block around it, are known: a type written there reads at
                    // them (body.rs `env_at`). A chain is at most as long as the body's scopes.
                    let mut sc = Some(s as u32);
                    let mut b = None;
                    for _ in 0..=scopes {
                        let Some(here) = sc else { break };
                        if let Some(hb) = table.hand_blocks.get(here as usize).copied().flatten() {
                            b = Some(hb);
                            break;
                        }
                        sc = view.scope(ScopeId(here)).ok().and_then(|x| x.parent).map(|p| p.0);
                    }
                    // A chain is at most as long as the body's blocks.
                    for _ in 0..nested.blocks.len() {
                        let Some(here) = b else { break };
                        if core::mem::replace(&mut known[here as usize], true) {
                            break;
                        }
                        b = nested.blocks[here as usize].parent;
                    }
                }
                rows.push(Row { scope: s as u32, kind, name, key: parent, range, path: &[], abs: false, module: in_module });
                // The key: derived from the body's, by kind, name and ordinal among the rows
                // before it of the same kind and name (text order: no dependence on which body
                // was checked first).
                let last = rows.len() - 1;
                let ordinal = rows[..last].iter().filter(|r| r.kind == kind && r.name == name).count() as u32;
                let key = super::nested_exports::local_key(crate_key, parent, kind.tag(), name, ordinal);
                rows[last].key = key;
                let ck = keys.local(key, kind).map_err(HeadError::Key)?;
                self.heads.insert(ck, LocalHead { lib: key, kind, text, module: at.module, names_at, enclosing: at.owner, scope: s as u32, range, marks });
            }
        }
        let rows: &'n [Row<'n>] = frame.alloc_slice_copy(&rows);
        let mut blocks: blaze_own::Vec<BlockModule, &'n Frame> = blaze_own::Vec::new_in(frame);
        if let Some(ms) = modules.as_ref() {
            for (b, block) in nested.blocks.iter().enumerate() {
                if let (true, Some(module)) = (known[b], ms.get(b).copied().flatten()) {
                    blocks.push(BlockModule { open: block.open, close: block.close, module });
                }
            }
        }
        let blocks: &'n [BlockModule] = frame.alloc_slice_copy(&blocks);
        let scopes: &'n [super::scope_table::ScopeAt] = frame.alloc_slice_copy(&table.scopes);
        self.bodies.insert(at.owner, BodyLocals { parents, rows, blocks, scopes });
        Ok(())
    }

    /// The library's key of the body `at` when it is an item or a member of the names
    /// library: the item whose text is the body's text itself (the same slice), or the member
    /// of `at.item` (a member's body is kept under its container) whose text is. `None`: a
    /// body the library does not list under `at.item` (a local item's own body).
    fn library_body<S: NamesSource<'n>>(names: S, at: Enclosing<'_>) -> Option<LibKey> {
        let same = |t: &str| core::ptr::eq(t.as_ptr(), at.text.as_ptr()) && t.len() == at.text.len();
        if names.text_of(at.item).is_some_and(|it| same(it.text)) {
            return Some(at.item);
        }
        // The first member whose text is the body's, in declaration order.
        let mut found = None;
        names.each_member(at.item, &mut |m| {
            if found.is_none() && same(m.text) {
                found = Some(m.key);
            }
        });
        found
    }

    /// The local item `name` names in `ns`, written in block `scope` of the body `owner`: the
    /// block's items, then each enclosing block's (innermost first); where the body is itself
    /// a local item's, then the blocks around its declaration. `None`: the module's names.
    pub fn find(&self, owner: ItemKey, scope: Option<u32>, name: &str, ns: LocalNs) -> Option<Row<'n>> {
        let mut body = owner;
        let mut at = scope;
        // Each step out leaves a local item's body or a body's anonymous constant: at most as
        // many steps as there are of both.
        for _ in 0..=self.heads.len() + self.anons.len() {
            if let Some(b) = self.bodies.get(&body) {
                let mut s = at;
                // A chain is at most as long as the body's blocks.
                for _ in 0..=b.parents.len() {
                    let Some(here) = s else { break };
                    let wanted = |r: &&Row<'n>| r.scope == here && r.name == name && if ns == LocalNs::Value { r.kind.in_values() } else { r.kind.in_types() };
                    if let Some(r) = b.rows.iter().find(wanted) {
                        return Some(*r);
                    }
                    s = b.parents.get(here as usize).copied().flatten();
                }
            }
            let (enclosing, scope) = match self.heads.get(&body) {
                Some(h) => (h.enclosing, h.scope),
                None => *self.anons.get(&body)?,
            };
            body = enclosing;
            at = Some(scope);
        }
        None
    }

    /// The local item's head, if `key` is one: its kind's outline entry, text and the module
    /// its head's names are read at ([`LocalHead::names_at`]: its block's).
    pub fn head_of(&self, key: LibKey) -> Option<Result<(ItemDetail, &'n str, LibKey), HeadError>> {
        let h = self.heads.get(&checker_of(key))?;
        Some(match h.kind.detail() {
            Some(d) => Ok((d, h.text, h.names_at)),
            None => Err(HeadError::Item(key, Why::Unsupported(h.kind.form(), h.range))),
        })
    }
}

/// A `use` item's tree: its text after `use`, without the `;` (attributes and a visibility
/// before `use` dropped).
pub(super) fn use_tree(text: &str) -> &str {
    let t = text.trim_end();
    let t = t.strip_suffix(';').unwrap_or(t);
    let mut from = 0;
    // The first `use` that is a whole word.
    let bytes = t.as_bytes();
    while let Some(i) = t[from..].find("use") {
        let at = from + i;
        let before = at == 0 || !(bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_');
        let after = bytes.get(at + 3).is_none_or(|c| !(c.is_ascii_alphanumeric() || *c == b'_'));
        if before && after {
            return t[at + 3..].trim();
        }
        from = at + 3;
    }
    t.trim()
}
