//! Where liveness starts: the entry function, what is public at `Reachable`, what an
//! `#[allow(dead_code)]` or `#[expect(dead_code)]` covers, what the linker or the compiler needs
//! (`#[used]`, `#[no_mangle]`, `#[export_name]`, a lang item), `const _`, the tests of a test
//! build; and the items of trait impls, live once their trait (or trait item) and self type are.
//!
//! The attributes are read from hir-def's `AttrFlags` (one memoized bitset per definition), and
//! each definition's level and `has_allow_dead_code_or_lang_attr` are computed once and kept.
//!
//! Follows `compiler/rustc_passes/src/dead.rs` (`create_and_seed_worklist`,
//! `maybe_record_as_seed`, `has_allow_dead_code_or_lang_attr`) and
//! `compiler/rustc_middle/src/middle/codegen_fn_attrs.rs` (`contains_extern_indicator`)
//! (MIT or Apache-2.0).

use std::cell::Cell;

use base_db::TargetKind;
use cfg::CfgAtom;
use hir::{HirFileId, InFile, Name};
use hir_def::{AttrDefId, ModuleDefId, ModuleId, attrs::AttrFlags};
use hir_expand::MacroDefKind;
use intern::sym;
use syntax::view::{self as ast, AstNode as _};

use super::{
    defs::{DefIdx, Defs, Id, Kind, Parent},
    level::{LevelPlus, Levels},
    reach::Reach,
};
use crate::cx::Cx;

/// rustc's `WorkItem`: a definition to mark live and walk, whether what reached it came from an
/// `allow` or `expect` of `dead_code` (`propagated`), and whether it has one itself (`own`), so
/// that it is walked but not made live, for an `expect` to be fulfilled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct WorkItem {
    pub(super) id: DefIdx,
    pub(super) propagated: bool,
    pub(super) own: bool,
}

const _: () = assert!(size_of::<WorkItem>() == 8);

impl WorkItem {
    pub(super) fn new(id: DefIdx, comes_from_allow: bool) -> WorkItem {
        WorkItem { id, propagated: comes_from_allow, own: comes_from_allow }
    }
}

/// A trait impl or an item of one, live once what it needs is: its trait (for an impl) or its
/// trait's item (for an item), when local, and its self type, when a local ADT
/// (`check_impl_or_impl_item_live`), each resolved once.
#[derive(Clone, Copy, Debug)]
pub(super) struct Unsolved {
    pub(super) def: DefIdx,
    pub(super) trait_def: Option<DefIdx>,
    pub(super) self_adt: Option<DefIdx>,
}

pub(super) struct Seeds {
    pub(super) worklist: Vec<WorkItem>,
    /// Walked after the rest: what is public at `Reachable`, and what an `allow` or `expect`
    /// covers.
    pub(super) deferred: Vec<WorkItem>,
    pub(super) unsolved: Vec<Unsolved>,
}

/// What `allow_or_lang` gave, kept in one byte.
const NOT_ASKED: u8 = 0;
const NEITHER: u8 = 1;
const LANG: u8 = 2;
const ALLOW: u8 = 3;

/// What the pass reads of the crate, shared by its parts.
pub(super) struct Crate<'c, 'a, 'db> {
    pub(super) cx: &'c Cx<'a, 'db>,
    pub(super) defs: &'c Defs,
    pub(super) levels: &'c Levels<'a, 'db>,
    pub(super) reach: &'c Reach,
    /// rustc builds an executable of the crate: `None` where the front end does not know.
    pub(super) executable: Option<bool>,
    /// Of each definition, its level, once asked.
    level: Vec<Cell<Option<LevelPlus>>>,
    /// Of each definition, `allow_or_lang` once asked (`NOT_ASKED`, `NEITHER`, `LANG`, `ALLOW`).
    allow: Vec<Cell<u8>>,
}

impl<'c, 'a, 'db> Crate<'c, 'a, 'db> {
    pub(super) fn new(cx: &'c Cx<'a, 'db>, defs: &'c Defs, levels: &'c Levels<'a, 'db>, reach: &'c Reach) -> Self {
        Crate {
            cx,
            defs,
            levels,
            reach,
            executable: executable(cx),
            level: (0..defs.len()).map(|_| Cell::new(None)).collect(), // alloc: setup, a column sized once
            allow: (0..defs.len()).map(|_| Cell::new(NOT_ASKED)).collect(), // alloc: setup, a column sized once
        }
    }
}

impl<'db> Crate<'_, '_, 'db> {
    /// The ADT whose attributes set the level at `def` when it is what a builtin derive makes
    /// with no tokens (a sibling of its ADT in rustc's HIR, carrying the ADT's copied lint
    /// attributes).
    fn derived_from(&self, def: DefIdx) -> Option<InFile<ast::Node<'db>>> {
        let defs = self.defs;
        let derive = match defs[def].id {
            Id::Derive(it) | Id::DeriveMethod(it) => it,
            _ => return None,
        };
        let adt = defs.get(Id::Adt(derive.loc(self.cx.db).adt))?;
        Some(self.cx.syntax_view(defs[adt].node?))
    }

    /// The level of `dead_code` at `def` (rustc's `lint_level_spec_at_node`).
    pub(super) fn level(&self, def: DefIdx) -> LevelPlus {
        let slot = &self.level[def as usize];
        if let Some(it) = slot.get() {
            return it;
        }
        let level = match (self.defs[def].node, self.derived_from(def)) {
            (Some(node), _) => self.levels.at(self.cx, self.cx.syntax_view(node)),
            (None, Some(adt)) => self.levels.at_derived(self.cx, adt),
            (None, None) => self.levels.at(self.cx, self.crate_root()),
        };
        slot.set(Some(level));
        level
    }

    fn crate_root(&self) -> InFile<ast::Node<'db>> {
        let db = self.cx.db;
        let file: HirFileId = self.cx.krate.root_module(db).definition_source_file_id(db);
        InFile::new(file, self.cx.vsema.parse_or_expand(file))
    }

    /// rustc's `has_allow_dead_code_or_lang_attr`: `Some(true)` when `dead_code` is allowed or
    /// expected at `def`, `Some(false)` when the linker or the compiler needs it, else `None`.
    pub(super) fn allow_or_lang(&self, def: DefIdx) -> Option<bool> {
        let slot = &self.allow[def as usize];
        let it = match slot.get() {
            NOT_ASKED => {
                let it = self.ask_allow_or_lang(def);
                slot.set(it);
                it
            }
            it => it,
        };
        match it {
            ALLOW => Some(true),
            LANG => Some(false),
            _ => None,
        }
    }

    fn ask_allow_or_lang(&self, def: DefIdx) -> u8 {
        if self.level(def).is_allow_or_expect() {
            return ALLOW;
        }
        let db = self.cx.db;
        let d = &self.defs[def];
        let (owner, flags) = match d.id {
            Id::Field(it) => (None, AttrFlags::query_field(db, it)),
            id => match attr_owner(id) {
                Some(owner) => (Some(owner), AttrFlags::query(db, owner)),
                None => return NEITHER,
            },
        };
        let codegen = matches!(d.kind, Kind::Fn | Kind::AssocFn { .. } | Kind::Static);
        let extern_indicator = !d.foreign
            && (flags.intersects(AttrFlags::NO_MANGLE | AttrFlags::LINKED_BY_NAME | AttrFlags::HAS_EXPORT_NAME)
                || owner.and_then(|it| AttrFlags::linkage(db, it)).is_some_and(|it| it.as_str() != "internal"));
        let used_like = codegen && (extern_indicator || flags.contains(AttrFlags::USED));
        let lang = flags.contains(AttrFlags::LANG_ITEM)
            || flags.contains(AttrFlags::PANIC_HANDLER)
            || (d.foreign && d.kind == Kind::Fn && flags.contains(AttrFlags::RUSTC_CANONICAL_SYMBOL));
        if used_like || lang { LANG } else { NEITHER }
    }

    /// The item of the local trait that `def`, an item of a trait impl, implements.
    pub(super) fn trait_item_of(&self, def: DefIdx) -> Option<DefIdx> {
        self.defs.trait_item_of(def)
    }

    /// The local trait a trait impl is of.
    pub(super) fn impl_trait(&self, impl_: DefIdx) -> Option<DefIdx> {
        self.defs.impl_of(impl_)?.trait_
    }

    /// The local ADT an impl's self type is (rustc's `type_of(impl)` is `ty::Adt`), not behind
    /// a reference or in another type.
    pub(super) fn impl_self_adt(&self, impl_: DefIdx) -> Option<DefIdx> {
        self.defs.impl_of(impl_)?.self_adt
    }

    /// The function the root module names `main`, by one lookup of its scope.
    fn root_main(&self) -> Option<DefIdx> {
        let db = self.cx.db;
        let root: ModuleId = self.cx.krate.root_module(db).into();
        let scope = root.def_map(db).module(db, root).scope(db);
        match scope.get(&Name::new_symbol_root(sym::main)).values?.def {
            ModuleDefId::FunctionId(it) => self.defs.get(Id::Fn(it)),
            _ => None,
        }
    }

    /// rustc's `entry_fn`: the function marked `#[rustc_main]`, else the `main` the crate root
    /// names, of an executable without `#![no_main]`. Where the front end does not know whether
    /// the crate is an executable, its `main` is taken to be the entry, which can only make
    /// fewer items dead.
    fn entry(&self, root_main: Option<DefIdx>) -> Option<DefIdx> {
        if self.executable == Some(false) || crate::checks::has_crate_attr(self.cx, "no_main") {
            return None;
        }
        let db = self.cx.db;
        let rustc_main = self.defs.modules.iter().flat_map(|it| &it.free).copied().find(|&def| {
            let d = &self.defs[def];
            match (d.kind, d.id) {
                (Kind::Fn, Id::Fn(id)) => AttrFlags::query(db, id.into()).contains(AttrFlags::RUSTC_MAIN),
                _ => false,
            }
        });
        rustc_main.or(root_main)
    }

    pub(super) fn is_test_build(&self) -> bool {
        self.cx.krate.cfg(self.cx.db).check_atom(&CfgAtom::Flag(sym::test))
    }

    /// The `#[test]` and `#[bench]` functions, which under `--test` the harness's `main` calls
    /// through the constants it makes of them.
    fn tests(&self, out: &mut Vec<WorkItem>) {
        let db = self.cx.db;
        for def in self.defs.all() {
            let d = &self.defs[def];
            if d.kind != Kind::Fn {
                continue;
            }
            let Some(node) = d.node else { continue };
            let Some(call) = node.file_id.macro_file() else { continue };
            let test = match call.loc(db).def.kind {
                MacroDefKind::BuiltInAttr(_, it) => it.is_test() || it.is_bench(),
                _ => false,
            };
            if test && self.cx.syntax_view(node).value.parent().is_some_and(|it| it.parent().is_none()) {
                out.push(WorkItem::new(def, false));
            }
        }
    }
}

/// The attribute owner of a definition, for what has one.
fn attr_owner(id: Id) -> Option<AttrDefId> {
    Some(match id {
        Id::Fn(it) => it.into(),
        Id::Const(it) => it.into(),
        Id::Static(it) => it.into(),
        Id::Adt(it) => it.into(),
        Id::Variant(it) => it.into(),
        Id::Trait(it) => it.into(),
        Id::Alias(it) => it.into(),
        Id::Impl(it) => it.into(),
        Id::Field(_) | Id::Derive(_) | Id::DeriveMethod(_) | Id::Use(_) => return None,
    })
}

/// rustc's `crate_types().contains(&CrateType::Executable)`, from the target cargo builds, else
/// the crate's `#![crate_type]`s (none: `bin`).
pub(super) fn executable(cx: &Cx<'_, '_>) -> Option<bool> {
    match cx.krate.target_kind(cx.db) {
        TargetKind::Bin | TargetKind::Test | TargetKind::Bench | TargetKind::BuildScript => Some(true),
        TargetKind::Lib { .. } => Some(false),
        TargetKind::Example => None,
        TargetKind::Other => {
            let mut any = false;
            let mut bin = false;
            for meta in crate::checks::crate_attrs(cx) {
                if meta.simple_name().is_some_and(|it| it == "crate_type")
                    && let Some(value) = string_value(meta.syntax())
                {
                    any = true;
                    bin |= value == "bin";
                }
            }
            Some(!any || bin)
        }
    }
}

/// rustc's `create_and_seed_worklist`.
pub(super) fn seeds(k: &Crate<'_, '_, '_>) -> Seeds {
    let defs = k.defs;
    // alloc: setup, the three worklists, once per crate, sized from the numbering so they
    // rarely grow (each owner is unsolved at most once).
    let mut seeds = Seeds {
        worklist: Vec::with_capacity(defs.len()),
        deferred: Vec::with_capacity(defs.len() + defs.owners.len()),
        unsolved: Vec::with_capacity(defs.owners.len()),
    };
    let root_main = k.root_main();
    if let Some(main) = k.entry(root_main) {
        seeds.worklist.push(WorkItem::new(main, false));
    }
    if k.is_test_build() {
        if let Some(main) = root_main {
            seeds.worklist.push(WorkItem::new(main, false));
        }
        k.tests(&mut seeds.worklist);
    }
    for def in defs.all() {
        if k.reach.is_reachable(def) {
            seeds.deferred.push(WorkItem::new(def, false));
        }
    }
    for &owner in &defs.owners {
        maybe_record_as_seed(k, owner, &mut seeds);
    }
    seeds
}

/// rustc's `maybe_record_as_seed` of an owner (an item, a trait's or an impl's item, a foreign
/// item).
fn maybe_record_as_seed(k: &Crate<'_, '_, '_>, owner: DefIdx, seeds: &mut Seeds) {
    let defs = k.defs;
    let push = |item: WorkItem, seeds: &mut Seeds| match item.own {
        true => seeds.deferred.push(item),
        false => seeds.worklist.push(item),
    };
    let allow = k.allow_or_lang(owner);
    if let Some(comes_from_allow) = allow {
        push(WorkItem::new(owner, comes_from_allow), seeds);
    }
    let d = &defs[owner];
    match d.kind {
        Kind::Enum => {
            if let Some(comes_from_allow) = allow {
                for &variant in defs.children(owner) {
                    push(WorkItem::new(variant, comes_from_allow), seeds);
                }
            }
        }
        Kind::AssocFn { .. } | Kind::AssocConst | Kind::AssocTy if allow.is_none() => {
            let Parent::Def(parent) = d.parent else { return };
            if let Kind::Impl { of_trait: true } = defs[parent].kind {
                let trait_item = k.trait_item_of(owner);
                if let Some(trait_item) = trait_item
                    && let Some(comes_from_allow) = k.allow_or_lang(trait_item)
                {
                    push(WorkItem::new(owner, comes_from_allow), seeds);
                }
                seeds.unsolved.push(Unsolved { def: owner, trait_def: trait_item, self_adt: k.impl_self_adt(parent) });
            }
        }
        Kind::Impl { of_trait: true } if allow.is_none() => {
            let trait_ = k.impl_trait(owner);
            if let Some(trait_) = trait_
                && let Some(comes_from_allow) = k.allow_or_lang(trait_)
            {
                push(WorkItem::new(owner, comes_from_allow), seeds);
            }
            seeds.unsolved.push(Unsolved { def: owner, trait_def: trait_, self_adt: k.impl_self_adt(owner) });
        }
        Kind::Const if d.name.as_ref().is_some_and(|it| *it.symbol() == sym::underscore) => {
            push(WorkItem::new(owner, false), seeds)
        }
        _ => {}
    }
}

/// What is between the first and the last `"` of `node`: an attribute's string value.
fn string_value(node: ast::Node<'_>) -> Option<&str> {
    let text = node.text();
    let start = text.find('"')?;
    let end = text.rfind('"')?;
    (end > start).then(|| &text[start + 1..end])
}
