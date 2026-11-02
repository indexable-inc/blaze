//! What a dev run's start does for the dependency image: bind its open cells ([`plan`]).
//!
//! The image is a fixed map at its linked address inside a reservation of the workspace
//! executable ([`crate::map`]), so it slides with the executable. Everything else a start
//! does is the system loader's, on the executable itself, which the workspace executable's
//! link (blaze-link, beside the image's [`View`]) writes into it: the image's shared libraries
//! are among the executable's to load, its thread-locals are a part of the executable's own
//! thread-local block, its init and fini pieces are among the executable's in the whole
//! link's order, and its call frame information is in the executable's unwind tables.
//!
//! What is left is the cells the image's own link could not fill. Each of its binds
//! ([`crate::prelink::Bind`]) takes what a whole link resolves its name to: a strong
//! definition beats a weak one, of two weak ones the first on the link line wins, two strong
//! ones are the whole link's error. Candidates are the workspace's definition and the image's
//! own; with neither, the name is the shared libraries' ([`Lookup`]), 0 for a weak reference.

use crate::PremapError;
use crate::image::View;
use crate::prelink::BindKind;
use crate::table::{Def, Kind, Strength};

/// An 8-byte cell of the image the start writes before any of its code runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Write {
    /// Linked, as every address of the image.
    pub place: u64,
    pub value: u64,
    /// `value` is a linked address, which moves with the run's slide as `place` does; a
    /// thread-local's offset from the thread pointer does not.
    pub address: bool,
}

/// A cell bound to a name only a shared library defines: found in the run process's global
/// scope, which the system loader filled from the executable's libraries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Lookup<'a> {
    pub place: u64,
    pub name: &'a str,
    pub addend: i64,
    /// 0 when no library defines it; otherwise the run stops, naming it.
    pub weak: bool,
}

/// The image's open cells, bound.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Start<'a> {
    pub writes: Vec<Write>,
    pub lookups: Vec<Lookup<'a>>,
}

/// The binding of `image`'s open cells beside the workspace executable whose global names
/// are `workspace` (blaze-link's `premap::layout`), sorted by name, each once.
pub fn plan<'a>(image: &View<'a>, workspace: &[(String, Def)]) -> Result<Start<'a>, PremapError> {
    if !workspace.is_sorted_by(|a, b| a.0 < b.0) {
        return Err(PremapError::of(format_args!("the workspace link's names are not sorted, each once")));
    }
    let binds = image.binds().count();
    let mut writes = Vec::with_capacity(binds);
    let mut lookups = Vec::with_capacity(binds);
    for bind in image.binds() {
        let theirs = workspace.binary_search_by(|it| it.0.as_str().cmp(bind.name)).ok().map(|at| workspace[at].1);
        let def = match (theirs, bind.own) {
            (Some(a), Some(b)) if a.strength == Strength::Strong && b.strength == Strength::Strong => {
                return Err(PremapError::of(format_args!(
                    "`{}` is defined in the workspace and in a dependency, both strongly",
                    bind.name
                )));
            }
            (Some(a), Some(b)) => Some(match (a.strength, b.strength) {
                (Strength::Strong, _) => a,
                (_, Strength::Strong) => b,
                _ if b.order < a.order => b,
                _ => a,
            }),
            (one, other) => one.or(other),
        };
        match (def, bind.kind) {
            (Some(def), BindKind::Addr) if def.kind != Kind::ThreadLocal => {
                writes.push(Write { place: bind.place, value: def.value.wrapping_add_signed(bind.addend), address: true });
            }
            (Some(def), BindKind::TpOff) if def.kind == Kind::ThreadLocal => {
                writes.push(Write { place: bind.place, value: def.value.wrapping_add_signed(bind.addend), address: false });
            }
            (Some(_), _) => {
                return Err(PremapError::of(format_args!(
                    "`{}` is bound as {} and defined as {}",
                    bind.name,
                    if bind.kind == BindKind::TpOff { "a thread-local" } else { "an address" },
                    if bind.kind == BindKind::TpOff { "an address" } else { "a thread-local" },
                )));
            }
            (None, BindKind::TpOff) => {
                return Err(PremapError::of(format_args!(
                    "the dependency image reaches the thread-local `{}` of a shared library at a fixed \
                     offset, and a library the run loads has no fixed thread-local offset",
                    bind.name
                )));
            }
            (None, BindKind::Addr) => {
                lookups.push(Lookup { place: bind.place, name: bind.name, addend: bind.addend, weak: bind.weak });
            }
        }
    }
    Ok(Start { writes, lookups })
}
