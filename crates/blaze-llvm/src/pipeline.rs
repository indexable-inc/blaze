//! The named custom pipeline (D100 point 7 (a)). It is LLVM's own `default<O2>`, with the
//! inliner in its bottom-up order and -O2's function simplification, minus exactly the passes
//! that infer a fact about a callee from an offered body. What those passes would have inferred,
//! blaze supplies as attributes instead (lower.rs `CalleeFacts`, and the signature's
//! `ParamFactsRec`). So a callee edited without changing its facts invalidates nobody.
//!
//! The source text is the pinned LLVM's own (23.1.4, indexable-inc/llvm `e152d9fa2bc2`), never
//! one written from memory. `default_pipeline` (vendor/llvm-sys `LLVMBlazeDefaultPipeline`:
//! PassBuilder `buildPerModuleDefaultPipeline(O2)` and `printPipeline`, what
//! `opt -print-pipeline-passes` prints) printed it on 2026-11-16, kept in
//! /Volumes/Projects/blaze-int-1112-notes/port27/llvm-O2-pipeline-23.1.4.txt. A target adds
//! passes of its own: arm64's text has `loop-idiom-vectorize` (AArch64's PassBuilder callback),
//! and x86-64's does not. Otherwise the two are equal. The texts below are those two with
//! [`REMOVED`] deleted and nothing else changed (tests/pipeline.rs checks both against the live
//! default, so an LLVM bump that changes the default fails there).
//!
//! The record key holds [`Pipeline::key`] (the name with its revision) in
//! `OptimizedKeyIn::pipeline`, and [`LLVM_VERSION`] in `OptimizedKeyIn::llvm`. A change to a
//! text bumps the revision.

use blaze_tier::Arch;

/// The pinned LLVM's version: the key's `llvm` part.
pub const LLVM_VERSION: (u32, u32, u32) = (23, 1, 4);

/// A named pipeline: the key holds its name, LLVM runs its text.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Pipeline {
    /// `default<O2>` without inference over offered bodies.
    BlazeO2,
}

impl Pipeline {
    /// The pass pipeline text `LLVMRunPasses` parses for `arch`.
    pub const fn passes(self, arch: Arch) -> &'static str {
        match (self, arch) {
            (Pipeline::BlazeO2, Arch::Arm64) => BLAZE_O2_ARM64,
            (Pipeline::BlazeO2, Arch::X86_64) => BLAZE_O2_X86_64,
        }
    }

    /// The name messages use.
    pub const fn name(self) -> &'static str {
        match self {
            Pipeline::BlazeO2 => "blaze-O2",
        }
    }

    /// The name and revision the record key holds (`OptimizedKeyIn::pipeline`).
    pub const fn key(self) -> &'static str {
        match self {
            Pipeline::BlazeO2 => "blaze-O2 r1",
        }
    }
}

/// The passes `default<O2>` runs that `blaze-O2` does not, in the default's order, each with
/// what it would have inferred about a callee from an offered body and what blaze supplies
/// instead. Every occurrence is removed: `function-attrs` runs twice in the CGSCC walk.
pub const REMOVED: [Removed; 7] = [
    Removed {
        pass: "ipsccp",
        infers: "a callee's constant return value and its arguments' constant ranges, solved across the call graph",
        supplied_by: "the signature's ParamFactsRec `range` on the return and parameters (LIR `sigs`/`param_facts`)",
    },
    Removed {
        pass: "require<globals-aa>",
        infers: "which globals a callee reads or writes, from its body (GlobalsAA's mod/ref summary)",
        supplied_by: "CalleeFacts `readonly`/`readnone` (blaze's effect facts)",
    },
    Removed {
        pass: "function-attrs<skip-non-recursive-function-attrs>",
        infers: "nounwind, memory(read/none), nocapture, nonnull/noalias returns of a callee from its body (bottom-up)",
        supplied_by: "CalleeFacts `nounwind`/`readonly`/`readnone`/`noalias_ret`; ParamFactsRec `captures(none)`/`nonnull`/`noalias`",
    },
    Removed {
        pass: "function-attrs",
        infers: "the same, run again after the CGSCC simplification",
        supplied_by: "the same records",
    },
    Removed {
        pass: "deadargelim",
        infers: "that a callee ignores an argument or its return, replacing it at call sites",
        supplied_by: "nothing: the signature is the record's; an unused argument stays passed",
    },
    Removed {
        pass: "rpo-function-attrs",
        infers: "norecurse of a callee, top-down from its callers' bodies",
        supplied_by: "nothing: norecurse only lets globalopt localise globals of `main`",
    },
    Removed {
        pass: "recompute-globalsaa",
        infers: "GlobalsAA's mod/ref summary again, for the vectorizers",
        supplied_by: "CalleeFacts `readonly`/`readnone`",
    },
];

/// One pass `blaze-O2` removes from `default<O2>`, and why.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Removed {
    /// As the default text prints it.
    pub pass: &'static str,
    pub infers: &'static str,
    pub supplied_by: &'static str,
}

/// The default's text up to where arm64's `loop-idiom-vectorize` stands.
macro_rules! head {
    () => {
        "memprof-remove-attributes,annotation2metadata,forceattrs,inferattrs,coro-early,function<eager-inv>(ee-instrument<>,lower-expect,simplifycfg<bonus-inst-threshold=1;no-forward-switch-cond;no-switch-range-to-icmp;no-switch-to-arithmetic;no-switch-to-lookup;keep-loops;no-hoist-common-insts;no-hoist-loads-stores-with-cond-faulting;no-sink-common-insts;speculate-blocks;simplify-cond-branch;no-speculate-unpredictables>,sroa<modify-cfg>,early-cse<>),openmp-opt,called-value-propagation,globalopt,function<eager-inv>(mem2reg,instcombine<max-iterations=1;no-verify-fixpoint>,simplifycfg<bonus-inst-threshold=1;no-forward-switch-cond;switch-range-to-icmp;no-switch-to-arithmetic;no-switch-to-lookup;keep-loops;no-hoist-common-insts;no-hoist-loads-stores-with-cond-faulting;no-sink-common-insts;speculate-blocks;simplify-cond-branch;no-speculate-unpredictables>),always-inline,function(invalidate<aa>),require<profile-summary>,cgscc(devirt<4>(inline,openmp-opt-cgscc,function<eager-inv;no-rerun>(sroa<modify-cfg>,early-cse<memssa>,speculative-execution<only-if-divergent-target>,jump-threading,correlated-propagation,jump-table-to-switch,simplifycfg<bonus-inst-threshold=1;no-forward-switch-cond;switch-range-to-icmp;no-switch-to-arithmetic;no-switch-to-lookup;keep-loops;no-hoist-common-insts;no-hoist-loads-stores-with-cond-faulting;no-sink-common-insts;speculate-blocks;simplify-cond-branch;no-speculate-unpredictables>,instcombine<max-iterations=1;no-verify-fixpoint>,aggressive-instcombine,libcalls-shrinkwrap,tailcallelim,simplifycfg<bonus-inst-threshold=1;no-forward-switch-cond;switch-range-to-icmp;no-switch-to-arithmetic;no-switch-to-lookup;keep-loops;no-hoist-common-insts;no-hoist-loads-stores-with-cond-faulting;no-sink-common-insts;speculate-blocks;simplify-cond-branch;no-speculate-unpredictables>,reassociate,constraint-elimination,loop-mssa(loop-instsimplify,loop-simplifycfg,licm<no-allowspeculation>,loop-rotate<header-duplication;no-prepare-for-lto;no-check-exit-count>,licm<allowspeculation>,simple-loop-unswitch<no-nontrivial;trivial>),simplifycfg<bonus-inst-threshold=1;no-forward-switch-cond;switch-range-to-icmp;no-switch-to-arithmetic;no-switch-to-lookup;keep-loops;no-hoist-common-insts;no-hoist-loads-stores-with-cond-faulting;no-sink-common-insts;speculate-blocks;simplify-cond-branch;no-speculate-unpredictables>,instcombine<max-iterations=1;no-verify-fixpoint>,loop(loop-idiom,indvars,extra-simple-loop-unswitch-passes,"
    };
}

/// The default's text after it.
macro_rules! tail {
    () => {
        "loop-deletion,loop-unroll-full),sroa<modify-cfg>,vector-combine,mldst-motion<no-split-footer-bb>,gvn<>,sccp,bdce,instcombine<max-iterations=1;no-verify-fixpoint>,dfa-jump-threading,jump-threading,correlated-propagation,adce,memcpyopt,dse,move-auto-init,loop-mssa(licm<allowspeculation>),coro-elide,simplifycfg<bonus-inst-threshold=1;no-forward-switch-cond;switch-range-to-icmp;switch-to-arithmetic;no-switch-to-lookup;keep-loops;hoist-common-insts;no-hoist-loads-stores-with-cond-faulting;sink-common-insts;speculate-blocks;simplify-cond-branch;no-speculate-unpredictables>,instcombine<max-iterations=1;no-verify-fixpoint>),function(require<should-not-run-function-passes>),coro-split,coro-annotation-elide)),coro-cleanup,globalopt,globaldce,elim-avail-extern,function<eager-inv>(drop-unnecessary-assumes,float2int,lower-constant-intrinsics,loop(loop-rotate<header-duplication;no-prepare-for-lto;check-exit-count>,loop-deletion,loop-interchange),loop-distribute,inject-tli-mappings,loop-vectorize<no-interleave-forced-only;no-vectorize-forced-only;>,drop-unnecessary-assumes,infer-alignment,loop-load-elim,instcombine<max-iterations=1;no-verify-fixpoint>,simplifycfg<bonus-inst-threshold=1;forward-switch-cond;switch-range-to-icmp;switch-to-arithmetic;switch-to-lookup;no-keep-loops;hoist-common-insts;no-hoist-loads-stores-with-cond-faulting;sink-common-insts;speculate-blocks;simplify-cond-branch;no-speculate-unpredictables>,slp-vectorizer,vector-combine,instcombine<max-iterations=1;no-verify-fixpoint>,loop-unroll<O2>,transform-warning,sroa<preserve-cfg;aggregate-to-vector>,infer-alignment,instcombine<max-iterations=1;no-verify-fixpoint>,loop-mssa(licm<allowspeculation>),alignment-from-assumptions,loop-sink,instsimplify,div-rem-pairs,mergeicmps,expand-memcmp,tailcallelim,simplifycfg<bonus-inst-threshold=1;no-forward-switch-cond;switch-range-to-icmp;switch-to-arithmetic;no-switch-to-lookup;keep-loops;no-hoist-common-insts;hoist-loads-stores-with-cond-faulting;no-sink-common-insts;speculate-blocks;simplify-cond-branch;speculate-unpredictables>),alloc-token,globaldce,constmerge,cg-profile,rel-lookup-table-converter,lower-comment-string,function(annotation-remarks)"
    };
}

/// `blaze-O2` for arm64: LLVM 23.1.4's arm64 `default<O2>` minus [`REMOVED`].
pub const BLAZE_O2_ARM64: &str = concat!(head!(), "loop-idiom-vectorize,", tail!());

/// `blaze-O2` for x86-64: LLVM 23.1.4's x86-64 `default<O2>` minus [`REMOVED`].
pub const BLAZE_O2_X86_64: &str = concat!(head!(), tail!());

/// The tokens of a pipeline text, in order. A token is a pass or adaptor with its `<...>`
/// parameters, or `(` or `)`; commas separate and are not tokens. This is what the tests compare
/// against the live default.
pub fn tokens(text: &str) -> Tokens<'_> {
    Tokens { text, at: 0 }
}

/// See [`tokens`].
pub struct Tokens<'t> {
    text: &'t str,
    at: usize,
}

impl<'t> Iterator for Tokens<'t> {
    type Item = &'t str;

    fn next(&mut self) -> Option<&'t str> {
        let bytes = self.text.as_bytes();
        while self.at < bytes.len() && bytes[self.at] == b',' {
            self.at += 1;
        }
        let start = self.at;
        let first = *bytes.get(start)?;
        if first == b'(' || first == b')' {
            self.at += 1;
            return Some(&self.text[start..self.at]);
        }
        let mut depth = 0u32;
        while let Some(&byte) = bytes.get(self.at) {
            match byte {
                b'<' => depth += 1,
                b'>' if depth > 0 => depth -= 1,
                b',' | b'(' | b')' if depth == 0 => break,
                _ => {}
            }
            self.at += 1;
        }
        Some(&self.text[start..self.at])
    }
}
