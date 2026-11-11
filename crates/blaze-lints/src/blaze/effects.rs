//! `nopanic`, `noalloc`, `nonblocking` (the old spellings; what a function may do, `#[blaze::may(..)]`,
//! is `may.rs`'s) and `pure` (lane pure's, docs/v2/plan/design/pure-fns.md) (`deny` by default; each fires only where the code
//! asks for it, docs/v2/plan/design/effects.md): a function marked `#[blaze::nopanic]`,
//! `#[blaze::noalloc]` or `#[blaze::nonblocking]` (or a method of an impl of, or a default
//! method of, a trait so marked) that may, through any call, panic, call the global allocator or
//! block the thread. Each refusal is one finding at the function, saying the shortest call chain
//! to what does it and the rule, with a note at each step's place (the call it makes, and at the
//! last, what it does), so each step has its file and line.
//!
//! What it reads: `hir_ty_pure`'s check of the definition (`check_generic`), the one walk
//! `#[blaze::pure]` uses, over the monomorphized runtime MIR; a call only an instance decides
//! (a type parameter's method) is checked at each instance the build makes. `nopanic` is held
//! to the profile the crate is built with: an optional overflow check counts when the crate has
//! `overflow-checks` (`crate_checks_overflow`), never by a guessed default.
//!
//! The attributes change diagnostics only: no codegen key reads their bits, so a function with
//! or without one builds byte for byte the same.

use hir_def::{
    AttrDefId, DefWithBodyId, FunctionId, HasModule, ItemContainerId, Lookup, attrs::PureAttr,
};
use hir_ty::db::HirDatabase as _;
use hir_ty_mir::mir::overflow::crate_checks_overflow;
use hir_ty_pure::{
    effect_attrs::{NOALLOC, NONBLOCKING, nopanic_for},
    effects::EffectDef,
};

use super::{Finding, Lint, facts};
use crate::{check::Cx, diag};

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    for lint in [Lint::Nopanic, Lint::Noalloc, Lint::Nonblocking, Lint::Pure] {
        if !cx.on_blaze(lint) {
            continue;
        }
        let Some((finding, steps)) = find(cx, lint) else { continue };
        let diagnostic = facts::report_one(cx, finding);
        for (file, range, text) in steps {
            diagnostic.span_note(diag::of_file(file, range), text);
        }
    }
}

/// A step of a refusal's chain at its place: the file, the range, and what the step does there.
pub(super) type StepNote = (span::FileId, syntax::TextRange, String);

/// The notes of `refusal`'s steps placed in `krate`'s own files (another crate's file may have
/// no name in the build, and the message already names each step): each step but the last
/// calls the next, the last does what the refusal is about.
pub(super) fn step_notes(
    db: &base_db::Db,
    krate: base_db::Crate,
    refusal: &hir_ty_pure::Refusal,
) -> Vec<StepNote> {
    let own = db.file_source_root(krate.data(db).root_file_id).source_root_id(db);
    let chain = &refusal.chain;
    chain
        .iter()
        .enumerate()
        .filter_map(|(at, step)| {
            let place = step.at.as_ref()?;
            if db.file_source_root(place.file).source_root_id(db) != own {
                return None;
            }
            let text = match chain.get(at + 1) {
                Some(next) => format!("`{}` calls `{}` here", step.name, next.name), // alloc: a note, once per step
                None => format!("`{}` {} here", step.name, refusal.what), // alloc: as above
            };
            Some((place.file, place.range, text))
        })
        .collect() // alloc: the notes of one refusal, reported at the edge
}

/// What `#[blaze::<effect>]` on `func`, or on the trait whose method it is or implements, says.
fn marked(db: &base_db::Db, func: FunctionId, effect: &EffectDef) -> Option<PureAttr> {
    if let Some(it) = (effect.attr)(db, AttrDefId::FunctionId(func)) {
        return Some(it);
    }
    let trait_ = match func.lookup(db).container {
        ItemContainerId::TraitId(it) => it,
        ItemContainerId::ImplId(imp) => db.impl_trait(imp)?.skip_binder().def_id.0,
        _ => return None,
    };
    (effect.attr)(db, AttrDefId::TraitId(trait_))
}

/// The refusal of `lint` of the body `cx` walks, if any, with its steps' notes.
fn find(cx: &Cx<'_, '_>, lint: Lint) -> Option<(Finding, Vec<StepNote>)> {
    let analysis = cx.analysis()?;
    let Ok(DefWithBodyId::FunctionId(func)) = DefWithBodyId::try_from(analysis.def()) else {
        return None;
    };
    let db = cx.db();
    // A trait's method with no body is a declaration: there is no code of it to check, and each
    // impl's method is checked in its place.
    if matches!(func.lookup(db).container, ItemContainerId::TraitId(_))
        && !hir_def::signatures::FunctionSignature::of(db, func).has_body()
    {
        return None;
    }
    let whole = cx.range(cx.root())?;
    let finding = |message: String| Finding { lint, range: whole, message, note: None };
    let name = func_name(db, func);
    let krate = HasModule::krate(&func, db);
    let (effect, profile): (&EffectDef, &str) = match lint {
        Lint::Nopanic => {
            // Asked only of a function marked `nopanic`, so a crate's profile is read only there.
            marked(db, func, nopanic_for(true))?;
            match crate_checks_overflow(db, krate) {
                Some(true) => {
                    (nopanic_for(true), " (checked as this crate builds: with overflow checks)")
                }
                Some(false) => {
                    (nopanic_for(false), " (checked as this crate builds: without overflow checks)")
                }
                None => {
                    let message = format!(
                        "`{name}` is `#[blaze::nopanic]`, but whether its crate keeps overflow checks is not known, so it can't be checked"
                    );
                    return Some((finding(message), Vec::new()));
                }
            }
        }
        Lint::Noalloc => (&NOALLOC, ""),
        Lint::Pure => (&hir_ty_pure::PURE, ""),
        _ => (&NONBLOCKING, ""),
    };
    match marked(db, func, effect)? {
        PureAttr::Trusted(_) => None,
        PureAttr::Malformed(text) => {
            let message = format!(
                "`#[blaze::{}({})]` on `{name}`: the attribute takes nothing, or `trusted = \"why\"`",
                effect.name,
                text.as_str()
            );
            Some((finding(message), Vec::new()))
        }
        PureAttr::Checked => {
            let refusal = hir_ty_pure::check_generic(db, func, effect).err()?;
            // alloc: a message
            let message = format!("{}{profile}", refusal.said());
            Some((finding(message), step_notes(db, krate, &refusal)))
        }
    }
}

fn func_name(db: &base_db::Db, func: FunctionId) -> String {
    hir_def::signatures::FunctionSignature::of(db, func).name.symbol().as_str().to_owned() // alloc: a message's part
}
