//! What a v2 unit's [`Spec`] decides of its IR, for the IR builder: v1 reads these from the
//! crate's cfg and the process's environment, a v2 unit from its spec alone.
//!
//! The IR builder reads [`building`] when it starts a function (`Recorder::new`): with
//! `Some`, the compiling crate's `-C debug-assertions` (what `ub_checks()` folds to) and panic
//! strategy are these, the bodies it reads are those [`Build::options`] says, and the only
//! callees it builds in are the `#[inline(always)]` ones of crates no edit reaches, which the
//! unit's references and key name ([`super::refs::built_in`]).

use std::cell::Cell;

use base_db::{Db, Crate};
use blaze_codegen::{
    Backend,
    settings::{Panic, Spec},
};
use hir_ty_mir::mir::{MirOptLevel, MirOptions};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Build {
    /// `-C debug-assertions` of the compiling crate (`Recorder::debug_assertions`).
    pub(crate) debug_assertions: bool,
    /// `panic = "abort"`: no landing pads.
    pub(crate) aborts: bool,
    /// The unit's backend optimizes (`Llvm { opt > 0 }`): its bodies are at rustc's MIR level
    /// 2, as rustc's above `-Copt-level=0`; else at 1.
    pub(crate) optimized: bool,
    /// The unit's bodies of crates other than the library's are MIR-inlined
    /// (`Spec::mir_inline`).
    pub(crate) inlines: bool,
}

impl Build {
    /// What the references of an instance are walked with ([`super::refs`]): the settings of
    /// a unit under which its IR asks for the most, so the walk lists what any unit asks for.
    /// Unwinding and debug assertions only add code; the passes of MIR level 2 that level 1
    /// lacks only remove it (`hir-ty-mir` `optimize`), all but the inliner, which
    /// [`Self::WALK_INLINED`] walks for.
    pub(crate) const WALK: Build =
        Build { debug_assertions: true, aborts: false, optimized: false, inlines: false };

    /// [`Self::WALK`] for an instance some unit of which MIR-inlines: the inlined body asks
    /// for what its callees' code does, which the body at level 1 calls instead.
    pub(crate) const WALK_INLINED: Build =
        Build { debug_assertions: true, aborts: false, optimized: true, inlines: true };

    pub(crate) fn of(spec: &Spec) -> Self {
        Build {
            debug_assertions: spec.debug_assertions,
            aborts: spec.panic == Panic::Abort,
            optimized: matches!(spec.backend, Backend::Llvm { opt } if opt > 0),
            // rustc's `Inline::is_enabled`, as the plan decides it (`Spec::mir_inline`: opt 2
            // and 3, not `s`/`z`, not a build that patches). A unit's key holds the code of what
            // it inlined (`super::refs::built_in`), so an edit of an inlined callee compiles its
            // callers again.
            inlines: spec.mir_inline,
        }
    }

    /// What a body of `written` is optimized with in a function compiled as `compiling`, by
    /// rustc's rules: the library's crates' MIR at level 2 and inlined, as shipped, and every
    /// other at the unit's level, inlined where the unit inlines ([`Self::inlines`]); UB
    /// checks as `sess.ub_checks()` of the crate whose MIR it is, which for the compiling
    /// crate is the unit's. The code generator always gives lifetimes.
    ///
    /// `hir-ty-mir`'s inliner gives a callee of another crate the options this gives it
    /// (`inline.rs` `callee_options`): the two change together.
    pub(crate) fn options(self, db: &Db, written: Crate, compiling: Crate) -> MirOptions {
        let lang = written.data(db).origin.is_lang();
        let level = match self.optimized || lang {
            true => MirOptLevel::Release,
            false => MirOptLevel::Debug,
        };
        let ub_checks = match written == compiling {
            true => self.debug_assertions && !lang,
            false => crate::debug_assertions(db, written),
        };
        let inline = lang || self.inlines;
        MirOptions { level, ub_checks, lifetime_markers: true, inline }
    }
}

thread_local! {
    static BUILDING: Cell<Option<Build>> = const { Cell::new(None) };
    static READ: Cell<bool> = const { Cell::new(false) };
}

/// The settings of the v2 unit whose IR the thread is building; `None` outside one.
pub(crate) fn building() -> Option<Build> {
    READ.set(true);
    BUILDING.get()
}

/// Runs `work`, which builds one function's IR, with `build` as what [`building`] says. An
/// IR builder that never asked would have built the function as v1 does: that is an error.
pub(crate) fn with<T>(build: Build, work: impl FnOnce() -> T) -> Result<T, String> {
    let held = (BUILDING.replace(Some(build)), READ.replace(false));
    let out = crate::tier::plain(work);
    let read = READ.replace(held.1);
    BUILDING.set(held.0);
    if !read {
        return Err("the IR builder did not read the unit's settings (`v2::build::building`), \
                    so it built the function with the crate's"
            .to_owned()); // alloc: the error's message
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_walks_differ_only_in_the_level_and_the_inliner() {
        let Build { debug_assertions, aborts, optimized, inlines } = Build::WALK_INLINED;
        assert_eq!((debug_assertions, aborts), (Build::WALK.debug_assertions, Build::WALK.aborts));
        assert!(optimized && inlines);
        assert!(!Build::WALK.optimized && !Build::WALK.inlines);
    }
}
