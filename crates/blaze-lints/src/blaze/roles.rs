//! `loop_turn_effects` and `sync_on_answer_path` (`deny` by default; each fires only where the
//! code asks for it, docs/v2/plan/design/effect-summary.md):
//!
//! - `loop_turn_effects`: a function marked `#[blaze::bounded_turn]`, a loop thread's turn,
//!   that may, through any call, block the thread, do IO, sync a file to the disk or take a
//!   lock;
//! - `sync_on_answer_path`: a function marked `#[blaze::answers_request]`, an answer path, that
//!   may, through any call, sync a file to the disk.
//!
//! What it reads is the one effect summary, through `hir_ty_pure::check_generic`, as `nopanic`
//! and the other effect attributes do: the refusal names the shortest call chain to what does
//! it, with a note at each step's place. What the walk can't see (a call it can't resolve, a
//! foreign function no table knows) is refused by name, never taken as clean.

use hir_def::{DefWithBodyId, HasModule, ItemContainerId, Lookup, attrs::PureAttr};
use hir_ty_pure::{
    effects::EffectDef,
    roles::{ANSWERS_REQUEST, BOUNDED_TURN},
};

use super::{
    Finding, Lint,
    effects::{StepNote, marked, step_notes},
    facts,
};
use crate::{check::Cx, diag};

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    for lint in [Lint::LoopTurnEffects, Lint::SyncOnAnswerPath] {
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

/// The refusal of `lint` of the function `cx` walks, if it is marked and refused.
fn find(cx: &Cx<'_, '_>, lint: Lint) -> Option<(Finding, Vec<StepNote>)> {
    let analysis = cx.analysis()?;
    let Ok(DefWithBodyId::FunctionId(func)) = DefWithBodyId::try_from(analysis.def()) else {
        return None;
    };
    let db = cx.db();
    if matches!(func.lookup(db).container, ItemContainerId::TraitId(_))
        && !hir_def::signatures::FunctionSignature::of(db, func).has_body()
    {
        return None;
    }
    let effect: &EffectDef = match lint {
        Lint::LoopTurnEffects => &BOUNDED_TURN,
        _ => &ANSWERS_REQUEST,
    };
    let whole = cx.range(cx.root())?;
    let finding = |message: String| Finding { lint, range: whole, message, note: None };
    match marked(db, func, effect)? {
        PureAttr::Trusted(_) => None,
        PureAttr::Malformed(text) => {
            let name = hir_def::signatures::FunctionSignature::of(db, func).name.symbol().as_str();
            // The text is the attribute's token tree, its delimiters with it.
            let message = format!(
                "`#[blaze::{}{}]` on `{name}`: the attribute takes nothing, or `trusted = \"why\"`",
                effect.name,
                text.as_str()
            ); // alloc: a message
            Some((finding(message), Vec::new())) // alloc: empty, allocates nothing
        }
        PureAttr::Checked => {
            let refusal = hir_ty_pure::check_generic(db, func, effect).err()?;
            let krate = HasModule::krate(&func, db);
            Some((finding(refusal.said()), step_notes(db, krate, &refusal)))
        }
    }
}
