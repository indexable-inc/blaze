//! The toolchain library's names resolved ONCE for many programs: every toolchain unit's names
//! ([`Library::solve`], `blaze_scope::apart`), then each program's own crate over them
//! ([`Library::inputs_for`], [`Library::names`]). What [`Inputs::resolve`] does for one program
//! in one call, cut where the program's text first matters: the runner of the execution rungs
//! (examples/front2_rungs.rs) resolves the library in its parent and each rung's crate in a
//! forked child.
//!
//! The programs share the stand-in's name and edition (the graph's node of the program's crate
//! is part of what the library was resolved under): another is refused by name
//! (`ApartError::Differs`), never resolved over the wrong library.

use blaze_own::Bump;
use blaze_scope::apart::{Apart, ApartError};
use blaze_scope::resolve::{CrateInput, Names};
use blaze_scope::task::{First, Serial};

use super::{Inputs, Shared, Texts, Toolchain, UserCrate};

/// Every toolchain unit's names, and the inputs they were resolved from.
pub struct Library<'t> {
    inputs: &'t Inputs<'t>,
    apart: Apart<'t>,
}

impl<'t> Library<'t> {
    /// The names of every crate of `crates` (`inputs.crates()`) but the program's (index 0),
    /// on this thread (`Serial`: no thread is started). `inputs`' program crate is a stand-in:
    /// its name and edition are read, its files are not.
    /// The names are held in `bump`, the caller's arena, alive as long as the library.
    pub fn solve(inputs: &'t Inputs<'t>, crates: &[CrateInput<'t>], bump: Bump<'t>) -> Result<Library<'t>, ApartError<'t>> {
        let apart = Apart::solve(crates, 0, inputs.sysroot, &inputs.texts, &Serial, First::Prefetch, bump)?;
        Ok(Library { inputs, apart })
    }

    /// The toolchain the library is of.
    pub fn toolchain(&self) -> &'t Toolchain {
        self.inputs.tc
    }

    /// The inputs of `user` over this library's toolchain: [`Inputs::new`]'s answer, the packs
    /// (verified when the library's inputs were made) not verified again.
    pub fn inputs_for(&self, user: &'t UserCrate<'t>) -> Inputs<'t> {
        let of = self.inputs;
        Inputs {
            texts: Texts { user: user.files, packs: Shared::Borrowed(&of.texts.packs) },
            features: Shared::Borrowed(&of.features),
            cfgs: Shared::Borrowed(&of.cfgs),
            deps: Shared::Borrowed(&of.deps),
            user,
            tc: of.tc,
            sysroot: of.sysroot,
        }
    }

    /// Every crate's names for the program of `inputs` ([`Library::inputs_for`]; `crates` its
    /// `crates()`): the program's crate resolved now, over the library. What
    /// [`Inputs::resolve`] answers for the same program.
    pub fn names(&self, inputs: &Inputs<'t>, crates: &[CrateInput<'t>]) -> Result<Names<'t>, ApartError<'t>> {
        self.apart.finish(crates, &inputs.texts, &Serial)
    }
}
