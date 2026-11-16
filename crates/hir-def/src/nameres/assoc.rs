//! A trait's and an impl's associated items, over the records: the members its module's
//! key-index page lists under it, in the order they are written ([`crate::key::members`]),
//! each named by its row of its file's items record. blaze-surface has no `AssocItems` record
//! yet; the page is its stand-in (reconnect.md, "What the new-layer crates lack"). An item a
//! macro call in the trait or impl made is in an `Expansion` record, not built: such an item's
//! name stops the task (`NotBuilt`), and no item is quietly missing.

use std::alloc::Global;

use base_db::{Db, OrNeed};
use blaze_surface::{ItemKind, Items};
use hir_expand::{AstId, MacroCallId, name::Name};
use syntax::ast;
use thin_vec::ThinVec;

use crate::{
    AssocItemId, ConstId, FunctionId, ImplId, TraitId, TypeAliasId,
    key::{self, FromKey},
    nameres::diagnostics::DefDiagnostics,
};

/// The associated items under `parent`, each with its name.
fn assoc_items(db: &Db, parent: base_db::ItemKey) -> Box<[(Name, AssocItemId)], Global> {
    let items = db.items();
    let members = key::members(db, parent);
    let mut out = Vec::with_capacity_in(members.len(), Global);
    for member in members {
        let item = match items.kind(member).or_need() {
            ItemKind::Function => AssocItemId::FunctionId(FunctionId::of_key(db, member)),
            ItemKind::Const => AssocItemId::ConstId(ConstId::of_key(db, member)),
            ItemKind::TypeAlias => AssocItemId::TypeAliasId(TypeAliasId::of_key(db, member)),
            other => panic!(
                "item {member:x}: a {} under the trait or impl {parent:x}, which holds only \
                 functions, consts and type aliases",
                other.name()
            ),
        };
        out.push((key::item_name(db, member), item));
    }
    out.into_boxed_slice()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraitItems {
    pub items: Box<[(Name, AssocItemId)], Global>,
    /// The macro calls in the trait's item list: in `Expansion` records, none listed here.
    pub macro_calls: ThinVec<(AstId<ast::Item>, MacroCallId)>,
}

#[blaze_memo::tracked]
impl TraitItems {
    #[inline]
    pub(crate) fn query(db: &Db, tr: TraitId) -> &TraitItems {
        &Self::query_with_diagnostics(db, tr).0
    }

    #[blaze_memo::tracked(returns(ref), owns, mem_tag = Nameres)]
    pub fn query_with_diagnostics(db: &Db, tr: TraitId) -> (TraitItems, DefDiagnostics) {
        let items = TraitItems { items: assoc_items(db, tr.key(db)), macro_calls: ThinVec::new() };
        (items, DefDiagnostics::none())
    }

    pub fn associated_types(&self) -> impl Iterator<Item = TypeAliasId> + '_ {
        self.items.iter().filter_map(|(_name, item)| match item {
            AssocItemId::TypeAliasId(t) => Some(*t),
            _ => None,
        })
    }

    pub fn associated_type_by_name(&self, name: &Name) -> Option<TypeAliasId> {
        self.items.iter().find_map(|(item_name, item)| match item {
            AssocItemId::TypeAliasId(t) if item_name == name => Some(*t),
            _ => None,
        })
    }

    /// The associated type a name looks up among these items, keyed for this trait
    /// ([`crate::nameres::def_site::assoc_item_is`]).
    pub fn associated_type_named(&self, db: &Db, name: Name) -> Option<TypeAliasId> {
        self.items.iter().find_map(|&(item_name, item)| match item {
            AssocItemId::TypeAliasId(t)
                if crate::nameres::def_site::assoc_item_is(db, item_name, item, name) =>
            {
                Some(t)
            }
            _ => None,
        })
    }

    pub fn method_by_name(&self, name: &Name) -> Option<FunctionId> {
        self.items.iter().find_map(|(item_name, item)| match item {
            AssocItemId::FunctionId(t) if item_name == name => Some(*t),
            _ => None,
        })
    }

    pub fn assoc_item_by_name(&self, name: &Name) -> Option<AssocItemId> {
        self.items.iter().find_map(|&(ref item_name, item)| (item_name == name).then_some(item))
    }

    pub fn macro_calls(&self) -> impl Iterator<Item = (AstId<ast::Item>, MacroCallId)> + '_ {
        self.macro_calls.iter().copied()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ImplItems {
    pub items: Box<[(Name, AssocItemId)], Global>,
    /// The macro calls in the impl's item list: in `Expansion` records, none listed here.
    pub macro_calls: ThinVec<(AstId<ast::Item>, MacroCallId)>,
}

#[blaze_memo::tracked]
impl ImplItems {
    #[blaze_memo::tracked(returns(ref), owns, mem_tag = Nameres)]
    pub fn of(db: &Db, id: ImplId) -> (ImplItems, DefDiagnostics) {
        let items = ImplItems { items: assoc_items(db, id.key(db)), macro_calls: ThinVec::new() };
        (items, DefDiagnostics::none())
    }
}

impl ImplItems {
    pub fn macro_calls(&self) -> impl Iterator<Item = (AstId<ast::Item>, MacroCallId)> + '_ {
        self.macro_calls.iter().copied()
    }
}
