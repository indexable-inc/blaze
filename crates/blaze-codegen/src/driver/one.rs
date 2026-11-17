//! One function instance compiled alone at the dev tier: the per-instance compile every caller
//! goes through, the driver's whole build ([`super::Build`]'s `compile_missing`), its lazy
//! compiles (`later::compiled`) and the no-daemon command's code task
//! (`blaze_command::code::code`), so there is one compile path and not two.
//!
//! The three steps, each a pure function of its inputs:
//!
//! - [`prepare_one`]: the IR of the instance against its places, and its shape as the memo keys
//!   it ([`Shape::relative`] when its own positions are by table, [`Shape::own_place`] when it
//!   calls itself);
//! - [`emit_one`]: the IR compiled with no imports (dev: no cross-function pass), refused when
//!   the backend compiled it under another key than the unit's;
//! - [`dev_template_of`]: the compiled function as the store keeps it, a [`Template`] against
//!   its places, with its side table's put.
//!
//! What stays the caller's: where the template is looked up first (store, memo, a flight of
//! another build), and the check that code built in is held by the key (`unkept_inline`),
//! which needs the build's other instances; a lone dev compile builds nothing in, which the
//! command checks itself.
//!
//! Invariant: a template made here, instantiated with the places it was made against, is the
//! function [`emit_one`] gave (`Template::of_owned_at`'s own contract).
//!
//! Floor: the backend's own prepare and emit of the one function; this file adds one key
//! comparison and one move of the side table.

use blaze_cas::Key;

use super::error;
use crate::{
    CodegenError, Target, Unit,
    backend::{Emitted, Frontend, Prepare, Shape},
    query::keys,
    settings::Spec,
    template::{OwnAnchors, Template, own_place},
};

/// A dev compile's result as the store keeps it: the template, and its side table (debug
/// info and panic locations) under its key, when the backend made one.
pub struct DevCompiled {
    pub template: Template,
    pub side: Option<(Key, Vec<u8>)>,
}

/// [`Prepare::prepare`] of `instance` as `unit` against `refs`, with its shape as the memo keys
/// it: relative when its own positions are by table (`anchored`, [`crate::backend::Described::anchors`]
/// is `Some`), and with its own place where it calls itself. Every prepare of a function goes
/// through here.
pub fn prepare_one<B: Prepare>(
    backend: &B,
    frontend: &B::Frontend,
    unit: &Unit,
    spec: &Spec,
    instance: &<B::Frontend as Frontend>::Instance,
    refs: &[Target],
    anchored: bool,
) -> Result<(Shape, B::Prepared), CodegenError> {
    let (shape, made) = backend.prepare(frontend, unit, spec, instance, refs)?;
    let own = own_place(unit.instance.key(), refs);
    Ok((shape.relative(anchored).own_place(own), made))
}

/// [`Prepare::emit`] of `prepared` with no imports, its error naming the function `name`
/// (rule 19), refused when the backend compiled it under another key than `unit`'s.
pub fn emit_one<B: Prepare>(
    backend: &B,
    unit: &Unit,
    spec: &Spec,
    prepared: B::Prepared,
    refs: &[Target],
    name: &str,
) -> Result<Emitted, CodegenError> {
    let emitted =
        backend.emit(unit, spec, prepared, refs, None).map_err(|why| super::naming(why, name))?;
    let key = unit.instance.key();
    if emitted.function.key != key {
        // alloc: an error
        return Err(error(key, unit.item, format!("compiled as {:?}", emitted.function.key)));
    }
    Ok(emitted)
}

/// The template of `emitted` against `refs`, its own positions by table where its span tables
/// are anchored at `anchors`, with its side table's put; the error names why it has none.
///
/// A compile whose side table and the function's record of it disagree (one without the
/// other) stops here and gives no template (rule 3, no-fallback). Until 2026-11-14 the
/// driver's `compile_missing` recorded that error and went on: it kept the template in the
/// memo and the store (`Out::dev_template`) and instantiated it, with no side table stored,
/// so every later build that hit the template linked a function whose positions and debug
/// info name a table that is not there, a refusal recorded once and served as a result
/// forever after. Nothing relied on going on: the error already failed the build that
/// recorded it, and a build waiting on the same shape's flight (`fly`'s `Landed`) finds no
/// template in the memo and records its own error, as it does for every other refused
/// compile of the shape.
pub fn dev_template_of(
    unit: &Unit,
    emitted: Emitted,
    refs: &[Target],
    anchors: OwnAnchors<'_>,
) -> Result<DevCompiled, CodegenError> {
    let key = unit.instance.key();
    let side = match (emitted.side, emitted.function.side) {
        (Some(bytes), Some(id)) => Some((keys::side(&id), bytes)),
        (None, None) => None,
        _ => {
            return Err(error(
                key,
                unit.item,
                "the compile's side table and the function's record of it disagree",
            ));
        }
    };
    let template = Template::of_owned_at(emitted.function, refs, &emitted.extra, anchors)
        // alloc: an error
        .map_err(|why| error(key, unit.item, why.to_string()))?;
    Ok(DevCompiled { template, side })
}
