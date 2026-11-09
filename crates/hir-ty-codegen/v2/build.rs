//! What a v2 unit's [`Spec`] decides of its IR, for the IR builder: v1 reads these from the
//! crate's cfg and the process's environment, a v2 unit from its spec alone.
//!
//! The IR builder is handed the unit's [`Build`] as an argument (`backend::built`'s `build`,
//! `Recorder::new`'s `unit`), never through the thread: a compile waits in queries, and a fiber
//! resumes on any thread, or interleaves with another compile on its own (rule 27). Every body
//! it reads is the MIR query's under the options [`Build::options`] derives, which are that
//! query's key, so a body built under one unit's settings is never served for another's (rule
//! 18). With `Some`, the compiling crate's `-C debug-assertions` (what `ub_checks()` folds to)
//! and panic strategy are these, and the only callees it builds in are the `#[inline(always)]`
//! ones of crates no edit reaches, which the unit's references and key name
//! ([`super::refs::built_in`]).

use base_db::{Crate, Db};
use blaze_codegen::{
    Backend,
    settings::{Panic, Spec},
};
use hir_ty_mir::mir::{MirOptLevel, MirOptions, devirt::DevirtWorld};

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
    /// The unit is a proc macro's wasm module's (`Spec::proc_macro_bridge`): a call of what
    /// [`super::wasm::bridged`] names is the bridge's import.
    pub(crate) bridged: bool,
    /// The program observes no allocation (`Spec::heap_to_stack`): the bodies of crates other
    /// than the library's move allocations that never leave their function to the stack.
    pub(crate) heap_to_stack: bool,
    /// The program's dyn facts, which a body at MIR level 2 with the inliner on is
    /// devirtualized against ([`MirOptions::world`]): set only for a unit of a final binary
    /// once the walk of the whole program is done (`super::devirt`), `None` in every walk.
    pub(crate) world: Option<DevirtWorld>,
}

impl Build {
    /// What the references of an instance are walked with ([`super::refs`]): the settings of
    /// a unit under which its IR asks for the most, so the walk lists what any unit asks for.
    /// Unwinding and debug assertions only add code; the passes of MIR level 2 that level 1
    /// lacks only remove it (`hir-ty-mir` `optimize`), all but the inliner, which
    /// [`Self::WALK_INLINED`] walks for. Moving allocations to the stack only removes
    /// references too (the allocation's calls, the box's free) but for the drop of a value
    /// the box held, which the walk lists beside the drop of every box ([`super::refs`]).
    pub(crate) const WALK: Build = Build {
        debug_assertions: true,
        aborts: false,
        optimized: false,
        inlines: false,
        bridged: false,
        heap_to_stack: false,
        world: None,
    };

    /// [`Self::WALK`] for an instance some unit of which MIR-inlines: the inlined body asks
    /// for what its callees' code does, which the body at level 1 calls instead.
    pub(crate) const WALK_INLINED: Build = Build {
        debug_assertions: true,
        aborts: false,
        optimized: true,
        inlines: true,
        bridged: false,
        heap_to_stack: false,
        world: None,
    };

    /// The walks for an instance some unit of which MIR-inlines ([`Self::WALK_INLINED`]):
    /// with debug assertions and without, as a release unit builds. Unlike the rest of what
    /// they decide, what the inliner takes is not monotone in them: a callee's UB checks
    /// (`MirOptions::ub_checks`, its `debug_assertions` for a body of the caller's crate)
    /// make it bigger, so without them a callee that stayed a call under [`Self::WALK_INLINED`]
    /// may be inlined, and its code then asks for what it calls (`blaze-mem`'s `os::reserve`
    /// at `--release` asked for core's `runtime`, which no walk listed). Each is walked, so
    /// the references list what a unit under either asks for.
    pub(crate) const WALKS_INLINED: [Build; 2] =
        [Self::WALK_INLINED, Build { debug_assertions: false, ..Self::WALK_INLINED }];

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
            bridged: spec.proc_macro_bridge,
            heap_to_stack: spec.heap_to_stack,
            world: None,
        }
    }

    /// What a body of `written` is optimized with, `per_user` where it is an instance each crate
    /// that uses it compiles (`Instance::compiled_by_user`: a generic, a closure, an `#[inline]`
    /// function), by rustc's rules: the library's crates' MIR at level 2 and inlined, as
    /// shipped, and every other at the unit's level, inlined where the unit inlines
    /// ([`Self::inlines`]). The code generator always gives lifetimes. Allocations move to the
    /// stack in every crate but the library's, whose bodies are one for every program.
    ///
    /// UB checks (`CheckAlignment`, `CheckNull`) are `sess.ub_checks()` of the crate whose MIR
    /// it is, `written`: rustc runs them in that crate's `optimized_mir`, wherever the instance
    /// is later monomorphized or copied. A body compiled once, in its own crate's unit, has
    /// that unit's setting (as rustc's copy of it in that crate); one each user compiles has
    /// `written`'s own `-C debug-assertions`, as its cfg says, whichever unit or home crate
    /// compiles it: an `#[inline]` function of a dependency built without debug assertions
    /// keeps no alignment or null check in a member built with them. Never the home's
    /// (`hir_ty_mir::mir::home`), a choice of sharing that no store key names: the key (`code`,
    /// `args`, the unit's settings) covers `written`, whether the instance is compiled per user
    /// and this unit's setting, so one key is one body (rule 21). What
    /// `ub_checks()` itself is, the compiling unit's in the library's crates, is
    /// `ub_checks_in`'s, at code generation.
    ///
    /// `hir-ty-mir`'s inliner gives a callee of another crate the options this gives it
    /// (`inline.rs` `callee_options`): the two change together. Neither moves an allocation
    /// of a body built into another to the stack: the inliner's callees have
    /// `heap_to_stack: false`, and a body this side builds into its caller is built under
    /// [`Self::built_in`].
    pub(crate) fn options(self, db: &Db, written: Crate, per_user: bool) -> MirOptions {
        let lang = written.data(db).origin.is_lang();
        let level = match self.optimized || lang {
            true => MirOptLevel::Release,
            false => MirOptLevel::Debug,
        };
        let ub_checks = match per_user {
            false => self.debug_assertions && !lang,
            true => crate::debug_assertions(db, written),
        };
        let inline = lang || self.inlines;
        let heap_to_stack = self.heap_to_stack && !lang;
        // Devirtualized only where `hir-ty-mir` runs `Devirtualize` (level 2, inlined), and
        // never in the library's bodies, which are one for every program.
        let world = self.world.filter(|_| level == MirOptLevel::Release && inline && !lang);
        MirOptions { level, ub_checks, lifetime_markers: true, inline, heap_to_stack, world }
    }
}

impl Build {
    /// These settings for a body built into the body of its caller (`always_inlinable`): the
    /// ones whose [`Self::options`] are [`MirOptions::built_in`] of these, the one rule for a
    /// copy of a body in another, which `hir-ty-mir`'s inliner follows too. It reads no dyn
    /// facts and moves no allocation to the stack.
    pub(crate) fn built_in(self) -> Build {
        Build { world: None, heap_to_stack: false, ..self }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_walks_differ_only_in_the_level_and_the_inliner() {
        let Build { debug_assertions, aborts, optimized, inlines, bridged, heap_to_stack, world } =
            Build::WALK_INLINED;
        assert_eq!((debug_assertions, aborts), (Build::WALK.debug_assertions, Build::WALK.aborts));
        assert_eq!((bridged, heap_to_stack), (Build::WALK.bridged, Build::WALK.heap_to_stack));
        assert_eq!((world, Build::WALK.world), (None, None));
        assert!(optimized && inlines);
        assert!(!Build::WALK.optimized && !Build::WALK.inlines);
    }
}
