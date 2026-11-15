//! The errors rustc gives of items, names, attributes and impls that the front end reports
//! nothing of: each module a set of rustc's checks, computed from `hir`'s public queries,
//! each `pub(crate) fn check(cx) -> Vec<Found>` in the order rustc gives them within its pass.
//! A check that needs inference or trait solving is not made here: it is the front end's, and
//! asked of it (`fed1eb428d:docs/v2/requests/diagnostics.md`).

mod ast_validation;
mod attrs;
pub(crate) mod defs;
pub(crate) mod features;
pub(crate) mod impls;
mod krate;
mod macros;
pub(crate) mod names;
pub(crate) mod settings;
#[cfg(test)]
mod tests;

pub(crate) use defs::{block_conflicts, head_range_view};

use std::ops::ControlFlow;

use syntax::view::{self, HasAttrs as _};

use crate::{
    check::crate_pass,
    cx::{Beyond, Cx},
    lints::syntax::attrs::{Owner, invoking_attrs, owners as owners_of},
    lower::{Found, Phase},
    pieces::Piece,
};

/// rustc's `misc_checking_1` (the entry point, `check_mod_attrs`): after the resolver and the
/// early lints, before the checks of items. Every check of that pass is given at this phase.
pub(crate) const PASSES: Phase = Phase::Items;

/// What rustc says of `cx.krate` that `hir` does not, in rustc's order: `#![feature]` as
/// expansion first reads it, the definitions of `macro_rules!` and the gates of macro calls'
/// attributes (as expansion meets them), AST validation, the resolver's, the feature gates
/// (with the early lints), what attribute parsing says as the AST is lowered,
/// `misc_checking_1` (the entry point, `check_mod_attrs`, the unused and stable features), the
/// checks of items.
///
/// What several checks read is made once here: the owners of attributes, the features and the
/// attribute parsers' `Session`, and the lowering of every attribute, which attribute parsing
/// and `check_mod_attrs` both read.
///
/// The passes of the syntax group (plan 12: `macros`, `feature expansion`, `ast validation`,
/// `feature gates`, `attributes`, `checked attributes`, each finding what it finds of a node
/// from the node's own syntax and the crate's settings) walk `syntax_cx`: the pieces of the
/// crate a by-piece check left unclean (`Cx::scoped`), or `cx` itself. What a piece's probe
/// runs of them is [`syntax_clean`]. The rest read the whole crate through `cx`: the checks
/// of the crate root's features, the definitions, the names, the crate, the unused features
/// and the impls. The crate's settings as its own walk reads them must be those a probe is
/// given ([`settings::crate_settings`]): when they are not, the check stops, naming both.
///
/// Each statement group runs under its span `crate pass` ([`crate_pass`]), in the order
/// written: `owners and features`, `scoped owners` (of a by-piece check), `macros`, `feature expansion`, `ast validation`, `defs`,
/// `names`, `feature gates`, `attributes`, `crate`, `checked attributes`, `unused features`,
/// `impls`.
pub(crate) fn check<'db>(cx: &Cx<'_, 'db>, syntax_cx: &Cx<'_, 'db>) -> Vec<Found> {
    let flags = features::Flags::default();
    let (owners, session, mut found) = crate_pass(cx, "owners and features", || {
        let owners: Vec<Owner<'db>> = owners_of(cx, cx.walk());
        let (features, found) = features::configured(cx, &owners, &flags);
        let session = attrs::Session::of(cx, &owners, features);
        let root = cx.krate.root_module(cx.db).as_source_file_id(cx.db);
        let read = settings::CrateSettings::of_parts(root, &session.features, session.tools());
        let given = cx.settings();
        assert!(
            *given == read,
            "{}",
            blaze_report::say!(
                "the settings of crate ",
                blaze_report::say::Code(&cx.crate_name(cx.krate)),
                " its root's outline gives a piece's probe, ",
                blaze_report::say::Words(&format!("{given:?}")),
                ", are not those its walk reads, ",
                blaze_report::say::Words(&format!("{read:?}")),
                ": a by-piece check would run its passes with other settings than the whole check",
            )
            .flat(),
        );
        (owners, session, found)
    });
    let features = &session.features;
    let walk = syntax_cx.walk();
    let scoped: Option<Vec<Owner<'db>>> = match std::ptr::addr_eq(cx, syntax_cx) {
        true => None,
        false => Some(crate_pass(cx, "scoped owners", || owners_of(syntax_cx, walk))),
    };
    let syntax_owners: &[Owner<'db>] = scoped.as_deref().unwrap_or(&owners);
    found.extend(crate_pass(cx, "macros", || macros::check(syntax_cx, walk)));
    found.extend(crate_pass(cx, "feature expansion", || {
        features::expansion(syntax_cx, walk, syntax_owners, features)
    }));
    found.extend(crate_pass(cx, "ast validation", || {
        ast_validation::check(syntax_cx, walk, features)
    }));
    let scopes = crate_pass(cx, "defs", || {
        let scopes = defs::scopes(cx);
        found.extend(defs::check(cx, &scopes));
        scopes
    });
    found.extend(crate_pass(cx, "names", || names::check(cx, &scopes)));
    found.extend(crate_pass(cx, "feature gates", || {
        let mut found = features::staged(cx, &owners, features, &flags);
        found.extend(features::gates(syntax_cx, walk, features, &flags));
        found
    }));
    let lowered = crate_pass(cx, "attributes", || {
        attrs::invoked(syntax_cx, &invoking_attrs(syntax_cx, walk), &mut found);
        attrs::parsed(syntax_cx, &session, syntax_owners, &mut found)
    });
    found.extend(crate_pass(cx, "crate", || krate::check(cx)));
    crate_pass(cx, "checked attributes", || {
        attrs::checked(syntax_cx, &session, syntax_owners, &lowered, &mut found);
    });
    found.extend(crate_pass(cx, "unused features", || {
        features::unused_or_stable(cx, &owners, features)
    }));
    found.extend(crate_pass(cx, "impls", || impls::check(cx, &scopes)));
    found
}

/// Whether the passes of the syntax group of [`check`] find nothing in the walk of `cx`, a
/// piece's probe (`pieces::piece_clean`), each given what the check gives it, from the crate's
/// settings ([`settings::crate_settings`]) where the check reads the crate root. What none of
/// them can be given makes the probe stop ([`Cx::note_beyond`]), never answer:
/// - a crate with `#![feature(staged_api)]`, whose stability checks (`attrs::checked`) read
///   every owner of the crate at once;
/// - an outline of a module's file with an inner attribute that is not a doc comment, which
///   the check reads as its `mod` item's, in the file that declares it
///   (`lints::syntax::attrs::owners`): the sweep walks the two together
///   (`pieces::unclean`).
///
/// The checks of the crate root's features (`features::staged`, `unused_or_stable`) are the
/// crate root's, whose outline is never probed.
pub(crate) fn syntax_clean(cx: &Cx<'_, '_>) -> bool {
    let settings = cx.settings();
    if settings.enabled("staged_api") {
        cx.note_beyond(Beyond::Other);
    }
    if let Some(Piece::Outline(file)) = cx.probed_piece()
        && module_attrs_elsewhere(cx.db, file)
    {
        cx.note_beyond(Beyond::Other);
    }
    let flags = features::Flags::default();
    let walk = cx.walk();
    let owners = owners_of(cx, walk);
    let session = attrs::Session::of_settings(cx, settings);
    let features = &session.features;
    if !macros::check(cx, walk).is_empty()
        || !features::expansion(cx, walk, &owners, features).is_empty()
        || !ast_validation::check(cx, walk, features).is_empty()
        || !features::gates(cx, walk, features, &flags).is_empty()
    {
        return false;
    }
    // alloc: empty, allocates only when an attribute is in error
    let mut found = Vec::new();
    attrs::invoked(cx, &invoking_attrs(cx, walk), &mut found);
    let lowered = attrs::parsed(cx, &session, &owners, &mut found);
    attrs::checked(cx, &session, &owners, &lowered, &mut found);
    found.is_empty()
}

/// Whether the root of `file`'s outline has an inner attribute that is not a doc comment:
/// one the check reads as the attribute of the `mod` item that declares the file, with that
/// item's own, so neither file's piece alone says what the check finds of them.
pub(crate) fn module_attrs_elsewhere(db: &base_db::Db, file: base_db::EditionedFileId) -> bool {
    hir_def::signatures::outline_root(db, file).attrs().any(|it| it.kind().is_inner())
}

/// The modules of `cx.krate` (not those of blocks), in the order rustc visits them: the crate
/// root, then each module before the modules in it, in the order they are declared.
pub(crate) fn modules(cx: &Cx<'_, '_>) -> Vec<hir::Module> {
    let db = cx.db;
    let def_map = hir_def::nameres::crate_def_map(db, cx.krate.base());
    // alloc: setup, once per check pass (a `Cx::modules()` cache is request 4)
    let mut order = Vec::new();
    // alloc: setup, as above
    let mut stack = vec![def_map.root_module_id()];
    while let Some(id) = stack.pop() {
        order.push(hir::Module::from(id));
        let start = stack.len();
        stack.extend(def_map.module(db, id).children(db).values().copied());
        stack[start..].reverse();
    }
    order
}

/// The crate's attributes as rustc reads them: the crate root's `#![..]`, then those of
/// `-Z crate-attr`, with `cfg_attr` expanded as the crate's cfg says.
pub(crate) fn crate_attrs<'db>(cx: &Cx<'_, 'db>) -> Vec<view::Meta<'db>> {
    // alloc: setup, the crate root's attributes, once per caller per check
    let mut metas = Vec::new();
    each_crate_attr::<()>(cx, |meta| {
        metas.push(meta);
        ControlFlow::Continue(())
    });
    metas
}

/// Each of [`crate_attrs`] in order, until `each` breaks; nothing gathered.
fn each_crate_attr<'db, B>(
    cx: &Cx<'_, 'db>,
    mut each: impl FnMut(view::Meta<'db>) -> ControlFlow<B>,
) -> Option<B> {
    let root = cx.krate.root_module(cx.db).as_source_file_id(cx.db)?;
    let tree = cx.vsema.parse(root);
    let attrs = tree
        .attrs()
        .chain(hir_def::attrs::cli_crate_attrs(cx.db, cx.krate.into()).map(|it| it.value));
    hir_expand::attrs::expand_cfg_attr_view(attrs, || cx.krate.cfg(cx.db), |meta, _| each(meta))
}

/// Whether the crate has the attribute `#![name]` (or `#![name(..)]`, `#![name = ..]`).
pub(crate) fn has_crate_attr(cx: &Cx<'_, '_>, name: &str) -> bool {
    each_crate_attr(cx, |meta| {
        let named = meta
            .path()
            .and_then(|path| path.as_single_name_ref())
            .is_some_and(|it| it.text() == name)
            && !matches!(meta, view::Meta::CfgMeta(_) | view::Meta::CfgAttrMeta(_));
        match named {
            true => ControlFlow::Break(()),
            false => ControlFlow::Continue(()),
        }
    })
    .is_some()
}
