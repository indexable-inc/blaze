//! Def-site hygiene (macros 2.0): the names of generic parameters, fields, variants and
//! associated items keyed by the context they are written with, and rustc's hygiene algebra
//! over expansions. Kept from the old item layer (its `def_site`), less the parts that walked
//! the whole-crate def map: where a path written by a `macro` is looked up
//! ([`hygienic_origin`]) and where its privacy is checked from ([`privacy_scope`]) need the
//! module that defines a macro's expansion, which the records give once `Expansion` records
//! exist (reconnect.md, "What the new-layer crates lack"); until then they stop with
//! `NotBuilt` for a context that carries an opaque mark, and are the identity for every name a
//! real file or a `macro_rules!` writes, which is all code but `decl_macro`'s.
//!
//! A port of rustc's rule (`rustc_resolve`'s `hygienic_lexical_parent` and `expn_def_scope`,
//! bindings keyed by `normalize_to_macros_2_0`). Every item of a real file is written with the
//! root context, so its key is `None` and it compares by its plain name.

use base_db::{Db, ItemKey, OrNeed, SurfaceError};
use blaze_surface::{Items, NotBuilt, Place};
use hir_expand::{HirFileId, MacroCallId, name::Name};
use intern::sym;
use span::{SyntaxContext, TextRange};
use syntax::view;

use crate::{ModuleId, expr_store::HygieneId, key::FromKey};

/// Where a path's first segment is looked up, after def-site hygiene.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hygienic {
    /// Where the path is written.
    Here,
}

/// Where the first segment of a path written with `hygiene` is looked up. A context with no
/// opaque mark (every real file's and every `macro_rules!`'s) is looked up where it is written.
/// One with an opaque mark is looked up at the module defining the `macro`, which needs its
/// expansion's record: a typed stop until those records exist.
pub(crate) fn hygienic_origin(db: &Db, here: ModuleId, hygiene: HygieneId) -> Hygienic {
    if hygiene.is_root() {
        return Hygienic::Here;
    }
    match key_of_context(db, hygiene.syntax_context()) {
        None => Hygienic::Here,
        Some(_) => not_built(db, here, "def-site lookup of a path a `macro` wrote"),
    }
}

/// A stop naming the module the question is asked in.
fn not_built<T>(db: &Db, here: ModuleId, method: &'static str) -> T {
    let item = crate::nameres::scope_key(db, here);
    Err(SurfaceError::NotBuilt(NotBuilt { method, item })).or_need()
}

// rustc's hygiene algebra (`rustc_span::hygiene`), over expansions as macro calls: `None` is
// rustc's `ExpnId::root()`, the expansion every real file is in.

/// The expansion the call `expn` is written in (rustc's `ExpnData::parent`): `None` for a call
/// written in a real file.
pub fn expn_parent(db: &Db, expn: MacroCallId) -> Option<MacroCallId> {
    expn.loc(db).kind.file_id().macro_file()
}

/// Whether `expn` is `ancestor` or was made inside it, at any depth (rustc's
/// `ExpnId::is_descendant_of`). Everything descends from the root, `None`.
pub fn is_descendant_of(db: &Db, expn: Option<MacroCallId>, ancestor: Option<MacroCallId>) -> bool {
    let Some(ancestor) = ancestor else { return true };
    let mut at = expn;
    // Each turn moves to the expansion a call is written in, ending at a real file (`None`).
    while let Some(here) = at {
        if here == ancestor {
            return true;
        }
        at = expn_parent(db, here);
    }
    false
}

/// The expansion whose output `file` is: `None` for a real file.
pub fn expansion_of(file: HirFileId) -> Option<MacroCallId> {
    file.macro_file()
}

/// The expansion an item is written in: `None` for an item of a file. An item an expansion
/// made names its expansion by the expansion's key, which maps to a call once `Expansion`
/// records exist: a typed stop until then.
pub fn item_expansion(db: &Db, key: ItemKey) -> Option<MacroCallId> {
    match db.items().place(key).or_need() {
        Place::File { .. } => None,
        Place::Expansion { .. } => {
            Err(SurfaceError::NotBuilt(NotBuilt { method: "the expansion of an expanded item", item: key }))
                .or_need()
        }
    }
}

/// rustc's `SyntaxContext::adjust`: removes the outer marks of `ctxt` until `expn` descends
/// from its outer expansion. Gives the last mark removed.
pub fn adjust(db: &Db, ctxt: &mut SyntaxContext, expn: Option<MacroCallId>) -> Option<MacroCallId> {
    let mut scope = None;
    // Each turn removes one mark; a root context has none and its outer expansion is the root,
    // which every expansion descends from.
    while !is_descendant_of(db, expn, ctxt.outer_expn(db).map(MacroCallId::from)) {
        scope = ctxt.remove_mark(db).0.map(MacroCallId::from);
    }
    scope
}

/// The key a name written with `ctxt` is looked up by in a scope that `expn` made (rustc's
/// `normalize_to_macros_2_0_and_adjust`). `None` where that is a root context.
pub fn key_in(db: &Db, ctxt: SyntaxContext, expn: Option<MacroCallId>) -> Option<SyntaxContext> {
    let mut ctxt = ctxt.normalize_to_macros_2_0(db);
    adjust(db, &mut ctxt, expn);
    (!ctxt.is_root()).then_some(ctxt)
}

/// rustc's `hygienic_eq`.
pub fn hygienic_eq(
    db: &Db,
    use_ctxt: SyntaxContext,
    def_key: Option<SyntaxContext>,
    expn: Option<MacroCallId>,
) -> bool {
    key_in(db, use_ctxt, expn) == def_key
}

/// The name a field or an associated item written `name` with `ctxt` is looked up by among
/// the fields of an ADT or the items of an impl that `expn` made.
pub fn name_in(db: &Db, name: Name, ctxt: SyntaxContext, expn: Option<MacroCallId>) -> Name {
    if ctxt.is_root() {
        return name;
    }
    match key_in(db, ctxt, expn) {
        Some(key) => name.keyed(key),
        None => name,
    }
}

/// rustc's `adjust_ident_and_get_scope`, its scope: the module the privacy of a field or
/// associated item named with `ctxt` is checked from. `None` where adjusting removes no mark
/// (every name a real file or a `macro_rules!` writes); else the module defining the macro of
/// the mark removed, which needs that expansion's record (a typed stop until it exists).
pub fn privacy_scope(
    db: &Db,
    ctxt: SyntaxContext,
    expn: Option<MacroCallId>,
    here: ModuleId,
) -> Option<ModuleId> {
    if ctxt.is_root() {
        return None;
    }
    let mut ctxt = ctxt.normalize_to_macros_2_0(db);
    adjust(db, &mut ctxt, expn)?;
    not_built(db, here, "def-site privacy of a name a `macro` wrote")
}

/// rustc's `expn_that_defined` of the ADT whose variant `id` is.
pub fn variant_expansion(db: &Db, id: crate::VariantId) -> Option<MacroCallId> {
    let key = match id {
        crate::VariantId::StructId(it) => it.key(db),
        crate::VariantId::UnionId(it) => it.key(db),
        crate::VariantId::EnumVariantId(it) => it.key(db),
    };
    item_expansion(db, key)
}

/// rustc's `expn_that_defined` of the enum `id`.
pub fn enum_expansion(db: &Db, id: crate::EnumId) -> Option<MacroCallId> {
    item_expansion(db, id.key(db))
}

/// The name a variant whose name is `name` (written in `file`) binds among its enum's
/// variants: keyed by the context it is written with.
pub(crate) fn variant_name(db: &Db, file: HirFileId, name: Option<view::Name<'_>>) -> Name {
    let Some(name) = name else { return Name::missing() };
    let plain = hir_expand::name::AsName::as_name(&name);
    match binding_key_at(db, file, view::AstNode::syntax(&name).text_range()) {
        Some(key) => plain.keyed(key),
        None => plain,
    }
}

/// rustc's `expn_that_defined` of the impl `id`.
pub fn impl_expansion(db: &Db, id: crate::ImplId) -> Option<MacroCallId> {
    item_expansion(db, id.key(db))
}

/// rustc's `expn_that_defined` of the trait `id`.
pub fn trait_expansion(db: &Db, id: crate::TraitId) -> Option<MacroCallId> {
    item_expansion(db, id.key(db))
}

/// The key the name of the associated item `item` binds among its impl's or trait's items:
/// `None` for an item of a file, whose name has the root context.
pub fn assoc_item_key(db: &Db, item: crate::AssocItemId) -> Option<SyntaxContext> {
    let key = match item {
        crate::AssocItemId::FunctionId(it) => it.key(db),
        crate::AssocItemId::ConstId(it) => it.key(db),
        crate::AssocItemId::TypeAliasId(it) => it.key(db),
    };
    // An expanded item's name key is read from its expansion, which is a stop until built.
    item_expansion(db, key).and(None)
}

/// [`name_in`] for a method name written with `hygiene` among the items of the impl `id`.
pub fn impl_item_name(db: &Db, id: crate::ImplId, name: Name, hygiene: HygieneId) -> Name {
    if hygiene.is_root() {
        return name;
    }
    name_in(db, name, hygiene.syntax_context(), impl_expansion(db, id))
}

/// Whether the associated item `item`, listed as `item_name`, is what `name` looks up.
pub fn assoc_item_is(db: &Db, item_name: Name, item: crate::AssocItemId, name: Name) -> bool {
    item_name.unkeyed() == name.unkeyed() && assoc_item_key(db, item) == name.key()
}

/// [`name_in`] for an associated item name written with `hygiene` among the items of the
/// trait `id`.
pub fn trait_item_name(db: &Db, id: crate::TraitId, name: Name, hygiene: HygieneId) -> Name {
    if hygiene.is_root() {
        return name;
    }
    name_in(db, name, hygiene.syntax_context(), trait_expansion(db, id))
}

/// `name`, the name of the associated item `item`, keyed by the item's own key.
pub fn assoc_item_binding_name(db: &Db, item: crate::AssocItemId, name: Name) -> Name {
    match assoc_item_key(db, item) {
        Some(key) => name.unkeyed().keyed(key),
        None => name.unkeyed(),
    }
}

/// The name an associated item written `name` with `hygiene` is asked for by where its trait
/// is not known yet (`T::Assoc`).
pub fn written_name(db: &Db, name: Name, hygiene: HygieneId) -> Name {
    if hygiene.is_root() {
        return name;
    }
    match key_of_context(db, hygiene.syntax_context()) {
        Some(key) => name.unkeyed().keyed(key),
        None => name,
    }
}

/// [`trait_item_name`] of a [`written_name`].
pub fn trait_item_written(db: &Db, id: crate::TraitId, written: Name) -> Name {
    match written.key() {
        Some(key) => name_in(db, written.unkeyed(), key, trait_expansion(db, id)),
        None => written,
    }
}

/// Whether `item`, a trait's item listed as `item_name`, is what the [`written_name`]
/// `written` looks up among that trait's items.
pub fn trait_item_is_written(
    db: &Db,
    item_name: Name,
    item: crate::AssocItemId,
    written: Name,
) -> bool {
    if item_name.unkeyed() != written.unkeyed() {
        return false;
    }
    let container = match item {
        crate::AssocItemId::FunctionId(it) => it.lookup(db).container,
        crate::AssocItemId::ConstId(it) => it.lookup(db).container,
        crate::AssocItemId::TypeAliasId(it) => it.lookup(db).container,
    };
    let crate::ItemContainerId::TraitId(trait_) = container else { return false };
    assoc_item_is(db, item_name, item, trait_item_written(db, trait_, written))
}

/// The key of a name written with `hygiene`, as a [`written_name`] has it.
pub fn written_key(db: &Db, hygiene: HygieneId) -> Option<SyntaxContext> {
    key_of_context(db, hygiene.syntax_context())
}

/// Whether the item `impl_item` of an impl of `trait_` implements the trait's `trait_item`
/// (rustc's `check_trait_item`).
pub fn implements(
    db: &Db,
    trait_: crate::TraitId,
    impl_item: crate::AssocItemId,
    impl_name: Name,
    trait_item: crate::AssocItemId,
    trait_name: Name,
) -> bool {
    impl_name.unkeyed() == trait_name.unkeyed()
        && implemented_name(db, || Some(trait_), impl_item, impl_name)
            == Some(assoc_item_binding_name(db, trait_item, trait_name))
}

/// The name the item `impl_item` of a trait impl implements among its trait's items.
pub fn implemented_name(
    db: &Db,
    trait_: impl FnOnce() -> Option<crate::TraitId>,
    impl_item: crate::AssocItemId,
    impl_name: Name,
) -> Option<Name> {
    Some(match assoc_item_key(db, impl_item) {
        Some(key) => name_in(db, impl_name.unkeyed(), key, trait_expansion(db, trait_()?)),
        None => impl_name.unkeyed(),
    })
}

/// The key a definition whose name is written with `ctxt` binds: its context normalized to
/// macros 2.0, `None` for a root context.
pub fn key_of_context(db: &Db, ctxt: SyntaxContext) -> Option<SyntaxContext> {
    if ctxt.is_root() {
        return None;
    }
    let key = ctxt.normalize_to_macros_2_0(db);
    (!key.is_root()).then_some(key)
}

/// The name a generic parameter written as `name` with `ctxt` is bound under, and the name a
/// path's first segment or a lifetime written with `ctxt` looks a parameter up by. `Self` is
/// never keyed (rustc: "FIXME(jseyfried) improve `Self` hygiene").
pub fn param_name(db: &Db, name: Name, ctxt: SyntaxContext) -> Name {
    if name == sym::Self_ {
        return name;
    }
    match key_of_context(db, ctxt) {
        Some(key) => name.keyed(key),
        None => name,
    }
}

/// [`param_name`] of a path's first segment written with `hygiene`.
pub fn path_param_name(db: &Db, name: Name, hygiene: HygieneId) -> Name {
    param_name(db, name, hygiene.syntax_context())
}

/// The key a binding whose name is written at `range` of `file` is keyed by.
pub fn binding_key_at(db: &Db, file: HirFileId, range: TextRange) -> Option<SyntaxContext> {
    if file.macro_file().is_none() {
        return None;
    }
    key_of_context(db, file.span_map(db).span_for_range(range).ctx)
}
