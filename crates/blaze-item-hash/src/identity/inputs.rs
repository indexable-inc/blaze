//! What a body's crate is built with, one hash per crate (the "crate inputs" part of a body's
//! key, `super::early_parts`), and the build of the front end itself. A lang item's signature
//! is not here: a step that reads one reads it as a fact.

use std::alloc::Global;

use base_db::{Crate, Db};
use blaze_ids::{ContentId, Hasher};
use hir_def::nameres::crate_def_map;
use hir_ty::next_solver::goal_store::{GoalFacts, solver_version};

const CRATE_INPUTS_DOMAIN: &str = "blaze v2 item identity crate inputs 1";

/// The stable id of the [`FrontEnd`] input's fact (its digest is the build itself).
const FRONT_END_FACT_DOMAIN: &str = "blaze v2 item identity front end input 1";

/// The build of the front end that computes identities: the content identity of the front
/// end's code and of how it was built (`blaze_codegen_code::FRONT_END`), as the goal store is
/// keyed. The driver sets it in the session arena while it builds a snapshot ([`set_build`]);
/// with none set, every identity is `Err`.
#[blaze_memo::input(singleton, debug)]
pub struct FrontEnd {
    #[returns(copy)]
    pub build: ContentId,
}

/// The fact of the [`FrontEnd`] input of `build`: one id for every build, its digest the build,
/// so a next snapshot with the same build reads the same fact.
pub fn build_fact(build: ContentId) -> blaze_memo::Fact {
    blaze_memo::Fact { id: ContentId::of(FRONT_END_FACT_DOMAIN, b"front end build"), digest: build }
}

/// Sets which front end computes identities in the snapshot being built: `inputs` is the
/// session arena's, `build` what `hir_ty::next_solver::goal_store::install` is given. What
/// [`give_build`] has the session arena run each time it is made.
pub fn set_build(inputs: &blaze_memo::Inputs<'_>, build: ContentId) {
    FrontEnd::set(inputs, build_fact(build), build);
}

/// Has the session arena set [`FrontEnd`] to `build` ([`set_build`]) each time it is made,
/// from `edit`'s commit on: how a driver or a test tells the database which front end computes
/// identities. A second call with the same build marks nothing (the maker's fact is the same).
pub fn give_build(edit: &mut base_db::Edit<'_>, build: ContentId) {
    edit.set_given(
        None,
        build_fact(build),
        Box::new_in(
            move |_: &blaze_memo::Builder, inputs: &blaze_memo::Inputs<'_>| set_build(inputs, build),
            Global,
        ),
    );
}

/// The build [`set_build`] set, `None` where the driver set none in this snapshot (a
/// snapshot's inputs are fixed, so the answer is too).
pub(crate) fn build(db: &Db) -> Option<ContentId> {
    FrontEnd::try_get(db).map(|front_end| front_end.build(db))
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
///   only what names resolve to, each a name fact of the step that looked it up);
/// - the borrow checker's mode (`hir_ty_borrowck::mode`, one per database).
///
/// MIR building reads no option of its own: overflow checks, debug assertions and the panic
/// strategy reach the front end as cfg, in the crate's key.
#[blaze_memo::tracked(returns(copy))]
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
    h.update(&[match hir_ty_borrowck::mode(db) {
        hir_ty_borrowck::Mode::Nll => 0,
        hir_ty_borrowck::Mode::Polonius => 1,
        hir_ty_borrowck::Mode::Off => 2,
        hir_ty_borrowck::Mode::PoloniusFlow => 3,
    }]);
    h.finish()
}
