//! Allocation budgets (`blaze-alloc-count`) of the reads a build makes per item in this crate's
//! attribute, loc, lang-item and upvar code: once warm, each is a memo hit, an intern hit or a
//! flag test, and allocates nothing.

use std::hint::black_box;

use base_db::{Crate, relevant_crates};
use either::Either;
use hir_expand::{Intern, Lookup};
use intern::{Symbol, sym};
use hir_expand::fixture;

use crate::{
    AdtId, AssocItemId, AttrDefId, DefWithBodyId, ExpressionStoreOwnerId, FieldId, FunctionId,
    ModuleDefId, StructId, VariantId,
    attrs::AttrFlags,
    lang_item::{LangItemEnum, crate_lang_items, lang_items},
    nameres::crate_def_map,
    test_db::TestDB,
    upvars::upvars_mentioned,
};

const CODE: &str = r#"
#![feature(lang_items)]
#[lang = "sized"]
pub trait Sized {}
#[lang = "copy"]
pub trait Copy: Clone {}
pub trait Clone {
    fn clone(&self) -> Self;
}

#[repr(C)]
pub struct Repr {
    pub a: u8,
    #[doc(hidden)]
    pub b: u16,
}
pub struct Plain(u32, u64);
pub enum Choice {
    A,
    #[deprecated]
    B(u8),
}
impl Clone for Plain {
    fn clone(&self) -> Self {
        Plain(self.0, self.1)
    }
}

#[export_name = "exported"]
pub fn exported() {}
#[inline(always)]
#[must_use = "use the sum"]
pub fn hot(x: u32) -> u32 {
    let add = |y: u32| x + y;
    add(1)
}
pub fn plain<T: Copy>(t: T) -> T {
    t
}
#[cfg_attr(all(), rustc_nounwind)]
pub fn no_unwind() {}
#[link_section = ".text.cold"]
pub fn sectioned() {}
pub const ONE: u32 = 1;
pub static TWO: u32 = 2;
pub type Alias = u32;
pub mod inner {
    pub fn nested() {}
    pub struct Unit;
}
"#;

/// The items of the fixture's crate, walked as `lang_item.rs` and `diagnostic_item.rs` walk a
/// crate: every module's declarations, enum variants, impls and trait and impl items.
#[derive(Default)]
struct Defs {
    items: Vec<AttrDefId>,
    functions: Vec<FunctionId>,
    structs: Vec<StructId>,
    adts: Vec<AdtId>,
    variants: Vec<VariantId>,
}

impl Defs {
    fn of(db: &TestDB, krate: Crate) -> Defs {
        let mut defs = Defs::default();
        for (module, data) in crate_def_map(db, krate).modules(db) {
            defs.items.push(module.into());
            let scope = data.scope(db);
            for def in scope.declarations() {
                match def {
                    ModuleDefId::FunctionId(it) => defs.function(it),
                    ModuleDefId::AdtId(it) => defs.adt(db, it),
                    ModuleDefId::TraitId(it) => {
                        defs.items.push(it.into());
                        for &(_, item) in it.trait_items(db).items.iter() {
                            defs.assoc(item);
                        }
                    }
                    ModuleDefId::ConstId(it) => defs.items.push(it.into()),
                    ModuleDefId::StaticId(it) => defs.items.push(it.into()),
                    ModuleDefId::TypeAliasId(it) => defs.items.push(it.into()),
                    ModuleDefId::MacroId(it) => defs.items.push(it.into()),
                    ModuleDefId::ModuleId(_)
                    | ModuleDefId::EnumVariantId(_)
                    | ModuleDefId::BuiltinType(_) => {}
                }
            }
            for it in scope.impls() {
                defs.items.push(it.into());
                for &(_, item) in it.impl_items(db).items.iter() {
                    defs.assoc(item);
                }
            }
        }
        defs
    }

    fn function(&mut self, it: FunctionId) {
        self.items.push(it.into());
        self.functions.push(it);
    }

    fn assoc(&mut self, item: AssocItemId) {
        match item {
            AssocItemId::FunctionId(it) => self.function(it),
            AssocItemId::ConstId(it) => self.items.push(it.into()),
            AssocItemId::TypeAliasId(it) => self.items.push(it.into()),
        }
    }

    fn adt(&mut self, db: &TestDB, it: AdtId) {
        self.items.push(it.into());
        self.adts.push(it);
        match it {
            AdtId::StructId(it) => {
                self.structs.push(it);
                self.variants.push(it.into());
            }
            AdtId::UnionId(it) => self.variants.push(it.into()),
            AdtId::EnumId(it) => {
                for &(variant, _) in it.enum_variants(db).variants.values() {
                    self.items.push(variant.into());
                    self.variants.push(variant.into());
                }
            }
        }
    }

    fn fields(&self, db: &TestDB) -> Vec<FieldId> {
        // alloc: test setup, outside the measure
        let mut fields = Vec::new();
        for &parent in &self.variants {
            for (local_id, _) in parent.fields(db).fields().iter() {
                fields.push(FieldId { parent, local_id });
            }
        }
        fields
    }
}

fn fixture() -> (TestDB, Crate) {
    let fx = fixture::db(CODE);
    let file = fx.only_file();
    let db = fx.into_db();
    let krate = relevant_crates(&db, file.file_id(&db))[0];
    (db, krate)
}

/// Budget: reading the flags of every item and field, and the words `has_word` answers from
/// them, is a memo hit and a bit test each: 0 allocations.
#[test]
fn attribute_flag_reads_allocate_nothing() {
    let (db, krate) = fixture();
    let defs = Defs::of(&db, krate);
    let fields = defs.fields(&db);
    assert!(defs.items.len() > 20 && fields.len() >= 5, "too small to measure");
    let has = |flag| defs.items.iter().any(|&it| AttrFlags::query(&db, it).contains(flag));
    for flag in [
        AttrFlags::LANG_ITEM,
        AttrFlags::HAS_REPR,
        AttrFlags::HAS_EXPORT_NAME,
        AttrFlags::HAS_LINK_SECTION,
        AttrFlags::IS_MUST_USE,
        AttrFlags::IS_INLINE,
        AttrFlags::RUSTC_NOUNWIND,
        AttrFlags::IS_DEPRECATED,
    ] {
        assert!(has(flag), "no item of the fixture has {flag:?}");
    }
    assert!(
        fields.iter().any(|&it| AttrFlags::query_field(&db, it).contains(AttrFlags::IS_DOC_HIDDEN)),
        "no field of the fixture is `#[doc(hidden)]`"
    );
    let read = || {
        for &item in &defs.items {
            black_box(AttrFlags::query(&db, item));
        }
        for &field in &fields {
            black_box(AttrFlags::query_field(&db, field));
        }
        for &function in &defs.functions {
            black_box(AttrFlags::is_rustc_nounwind(&db, function));
            for word in ["rustc_nounwind", "rustc_comptime", "panic_handler"] {
                black_box(AttrFlags::has_word(&db, function.into(), word));
            }
        }
    };
    read();
    blaze_alloc_count::assert_at_most(0, "reading the attribute flags of every item and field", read);
}

/// Budget: a value query gated by a flag is a bit test for an item without the attribute and a
/// memo hit for one with it, once warm: 0 allocations either way.
#[test]
fn gated_value_reads_allocate_nothing() {
    let (db, krate) = fixture();
    let defs = Defs::of(&db, krate);
    let read = || {
        for &item in &defs.items {
            let flags = AttrFlags::query(&db, item);
            black_box(AttrFlags::export_name(&db, item));
            black_box(AttrFlags::link_name(&db, item));
            black_box(AttrFlags::link_section(&db, item));
            black_box(AttrFlags::linkage(&db, item));
            black_box(AttrFlags::diagnostic_item(&db, item));
            black_box(AttrFlags::lang_item(&db, item));
            black_box(AttrFlags::must_use_message(&db, item));
            black_box(flags.unstable_feature(&db, item));
            black_box(flags.doc_aliases(&db, Either::Left(item)));
        }
        for &adt in &defs.adts {
            black_box(AttrFlags::repr(&db, adt));
            black_box(AttrFlags::rustc_layout_scalar_valid_range(&db, adt));
        }
        for &function in &defs.functions {
            black_box(AttrFlags::target_features(&db, function));
            black_box(AttrFlags::target_feature_list(&db, function));
            black_box(AttrFlags::define_opaque(&db, function));
        }
    };
    read();
    let named = |name: &str| {
        defs.items.iter().filter_map(|&it| AttrFlags::export_name(&db, it)).any(|it| it.as_str() == name)
    };
    assert!(named("exported"), "`#[export_name]` was not read");
    let sections: Vec<Symbol> =
        // alloc: test setup, outside the measure
        defs.items.iter().filter_map(|&it| AttrFlags::link_section(&db, it)).collect();
    assert_eq!(sections, [Symbol::intern(".text.cold")]);
    let reprs = defs.adts.iter().filter(|&&it| AttrFlags::repr(&db, it).is_some()).count();
    assert_eq!(reprs, 1, "only `Repr` has a `#[repr]`");
    let messages: Vec<&str> =
        // alloc: test setup, outside the measure
        defs.items.iter().filter_map(|&it| AttrFlags::must_use_message(&db, it).flatten()).collect();
    assert_eq!(messages, ["use the sum"]);
    blaze_alloc_count::assert_at_most(0, "the gated value reads of every item", read);
}

/// Budget: interning a loc that is already interned (every `ItemLoc` and `AssocItemLoc` of the
/// fixture) hashes its packed words and finds its slot: 0 allocations, the same ID back.
#[test]
fn interning_a_known_loc_allocates_nothing() {
    let (db, krate) = fixture();
    let defs = Defs::of(&db, krate);
    assert!(defs.functions.len() > 6 && defs.structs.len() >= 3, "too small to measure");
    // alloc: test setup, outside the measure
    let functions: Vec<_> = defs.functions.iter().map(|&it| (it, *it.lookup(&db))).collect();
    // alloc: test setup, outside the measure
    let structs: Vec<_> = defs.structs.iter().map(|&it| (it, *it.lookup(&db))).collect();
    blaze_alloc_count::assert_at_most(0, "interning every function's and struct's loc again", || {
        for &(id, loc) in &functions {
            assert_eq!(loc.intern(&db), id);
        }
        for &(id, loc) in &structs {
            assert_eq!(loc.intern(&db), id);
        }
    });
}

/// Budget: a lang item is a memo hit of the crate's table and a field load; a symbol to a lang
/// item is an index and a load: 0 allocations.
#[test]
fn lang_item_lookups_allocate_nothing() {
    let (db, krate) = fixture();
    let items = lang_items(&db, krate);
    assert!(items.Sized.is_some() && items.Copy.is_some(), "{items:?}");
    assert!(crate_lang_items(&db, krate).is_some());
    let symbols = [sym::sized, sym::copy, sym::drop, sym::fn_, Symbol::intern("not a lang item")];
    blaze_alloc_count::assert_at_most(0, "lang item lookups", || {
        let items = lang_items(&db, krate);
        black_box(items.Sized);
        black_box(items.Copy);
        black_box(crate_lang_items(&db, krate));
        for symbol in &symbols {
            if let Some(which) = LangItemEnum::from_symbol(symbol) {
                black_box(which.from_lang_items(items));
            }
        }
    });
}

/// Budget: the upvars of every body, once computed, are memo hits: 0 allocations. A body with no
/// closure has none and is never walked.
#[test]
fn upvar_reads_allocate_nothing() {
    let (db, krate) = fixture();
    let defs = Defs::of(&db, krate);
    let owners: Vec<ExpressionStoreOwnerId> = defs
        .functions
        .iter()
        .map(|&it| ExpressionStoreOwnerId::Body(DefWithBodyId::FunctionId(it)))
        // alloc: test setup, outside the measure
        .collect();
    let with_upvars = owners.iter().filter(|&&it| upvars_mentioned(&db, it).is_some()).count();
    assert_eq!(with_upvars, 1, "only `hot`'s closure captures a local");
    blaze_alloc_count::assert_at_most(0, "the upvars of every function", || {
        for &owner in &owners {
            black_box(upvars_mentioned(&db, owner));
        }
    });
}
