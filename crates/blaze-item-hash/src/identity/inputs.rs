//! Section 3.1: what a body's crate is built with, one hash per crate, and the build of the
//! front end itself.

use base_db::{Crate, Db};
use blaze_ids::{ContentId, Hasher};
use hir_def::{
    lang_item::{LangItemEnum, LangItemTarget, lang_items},
    nameres::crate_def_map,
};
use hir_ty::next_solver::goal_store::{GoalFacts, solver_version};
use blaze_query::db::{Database as _, Setter as _};

use super::item::{Part, item_part, variant_part};

const CRATE_INPUTS_DOMAIN: &str = "blaze v2 item identity crate inputs 1";
const LANG_ITEMS_DOMAIN: &str = "blaze v2 item identity lang items 1";

/// The build of the front end that computes identities: the toolchain and blaze's own code.
/// The driver tells it ([`set_build`]); with none told, every identity is `Err`.
#[blaze_query::input(singleton, debug)]
pub struct FrontEnd {
    #[returns(copy)]
    pub build: ContentId,
}

/// Tells identities which front end computes them: `build` is what
/// `hir_ty::next_solver::goal_store::install` is given. Telling the same build again changes
/// nothing.
pub fn set_build(db: &mut Db, build: ContentId) {
    match FrontEnd::try_get(db) {
        Some(front_end) if front_end.build(db) == build => {}
        Some(front_end) => {
            front_end.set_build(db).with_durability(blaze_query::db::Durability::HIGH).to(build);
        }
        None => {
            _ = FrontEnd::builder(build).durability(blaze_query::db::Durability::HIGH).new(db);
            // Reading a singleton that does not exist yet is not tracked: what read it before
            // reported an untracked read ([`build`]), which a new revision makes stale.
            db.storage_mut().synthetic_write(blaze_query::db::Durability::LOW);
        }
    }
}

/// The build [`set_build`] told, `None` before it was told.
pub(crate) fn build(db: &Db) -> Option<ContentId> {
    match FrontEnd::try_get(db) {
        Some(front_end) => Some(front_end.build(db)),
        None => {
            blaze_query::db::report_untracked_read(db);
            None
        }
    }
}

/// What every body of `krate` is inferred, lowered to MIR and borrow checked with, beyond its
/// own items (section 3.1):
///
/// - the crate's key, its lang items by key and the crates it loads, in order, by key: the goal
///   store's crate key (`blaze_items::goals::Goals`), which covers `lang_items` and
///   `loaded_crates`;
/// - its edition;
/// - its workspace data: the target (tuple, data layout, arch), the toolchain's version and
///   the const-eval budget, which covers `target_data_layout_query`;
/// - the front end's solver version (`solver_version` of the build), or that none is told;
/// - the attributes of its root: the `#![feature]` set, `#![register_tool]`s, `no_std`,
///   `no_core`, `proc-macro`, `rustc_coherence_is_core` and `recursion_limit`, which covers
///   `UnstableFeatures::query` and `crate_recursion_limit` (`no_implicit_prelude` changes
///   only what names resolve to, which section 3.5 hashes);
/// - the borrow checker's mode (`hir_ty_borrowck::mode`, one per process).
///
/// MIR building reads no option of its own: overflow checks, debug assertions and the panic
/// strategy reach the front end as cfg, in the crate's key.
#[blaze_query::tracked(returns(copy))]
pub fn crate_inputs(db: &Db, krate: Crate) -> ContentId {
    let mut h = Hasher::new(CRATE_INPUTS_DOMAIN);
    h.id(&blaze_items::goals::Goals.crate_key(db, Some(krate)));
    h.update(&[krate.data(db).edition as u8]);
    h.id(&krate.workspace_data(db).content());
    match build(db) {
        Some(build) => h.update(&[1]).id(&solver_version(&build)),
        None => h.update(&[0]),
    };
    let def_map = crate_def_map(db, krate);
    // A set: sorted by name, so the order of the attributes does not count.
    // alloc: the features' names, once per crate's inputs computed
    let mut features: Vec<_> = def_map.features().iter().collect();
    features.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    h.update(&(features.len() as u64).to_le_bytes());
    for feature in &features {
        h.part(feature.as_str().as_bytes());
    }
    let tools = def_map.registered_tools();
    h.update(&(tools.len() as u64).to_le_bytes());
    for tool in tools {
        h.part(tool.as_str().as_bytes());
    }
    h.update(&[
        def_map.is_no_std() as u8,
        def_map.is_no_core() as u8,
        def_map.is_proc_macro() as u8,
        def_map.is_rustc_coherence_is_core() as u8,
    ]);
    h.update(&def_map.recursion_limit().to_le_bytes());
    h.update(&[match hir_ty_borrowck::mode() {
        hir_ty_borrowck::Mode::Nll => 0,
        hir_ty_borrowck::Mode::Polonius => 1,
        hir_ty_borrowck::Mode::Off => 2,
    }]);
    h.finish()
}

/// Each lang item of `krate` by key and signature (section 3.1): what operators, `?`, `for`,
/// closures and `Box` are checked against. [`crate_inputs`] names them by key only.
#[blaze_query::tracked(returns(copy))]
pub(crate) fn lang_item_signatures(db: &Db, krate: Crate) -> Result<ContentId, &'static str> {
    let items = lang_items(db, krate);
    let mut h = Hasher::new(LANG_ITEMS_DOMAIN);
    for (at, item) in LangItemEnum::ALL.iter().enumerate() {
        let Some(target) = item.from_lang_items(items) else { continue };
        let part = lang_item_part(db, target)?;
        h.update(&(at as u32).to_le_bytes()).update(&part);
    }
    Ok(h.finish())
}

fn lang_item_part(db: &Db, target: LangItemTarget) -> Result<Part, &'static str> {
    use blaze_items::ItemId;
    let item = match target {
        LangItemTarget::EnumVariantId(it) => return variant_part(db, it),
        LangItemTarget::EnumId(it) => ItemId::Adt(it.into()),
        LangItemTarget::StructId(it) => ItemId::Adt(it.into()),
        LangItemTarget::UnionId(it) => ItemId::Adt(it.into()),
        LangItemTarget::FunctionId(it) => ItemId::Function(it),
        LangItemTarget::ImplId(it) => ItemId::Impl(it),
        LangItemTarget::StaticId(it) => ItemId::Static(it),
        LangItemTarget::TypeAliasId(it) => ItemId::Alias(it),
        LangItemTarget::TraitId(it) => ItemId::Trait(it),
        LangItemTarget::ConstId(it) => ItemId::Const(it),
        LangItemTarget::MacroId(it) => ItemId::Macro(it),
    };
    item_part(db, item)
}
