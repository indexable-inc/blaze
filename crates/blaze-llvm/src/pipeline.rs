//! The named custom pipeline (D100 point 7 (a)): LLVM's inliner in its bottom-up order with the
//! -O2 function simplification, and NO pass that infers a fact about a callee from an offered
//! body. What such passes would have inferred is supplied by blaze as attributes (lower.rs,
//! `CalleeFacts`), so a callee edited without changing its facts invalidates nobody.
//!
//! The pipeline's text is LLVM 23's `default<O2>` module pipeline as
//! `opt -passes='default<O2>' -print-pipeline-passes` prints it for the pinned LLVM, with these
//! removed: `ipsccp`; `function-attrs` and `rpo-function-attrs` (they run over the
//! `available_externally` bodies too); `attributor` (and `attributor-cgscc`); `globalopt` (its
//! inference over the offered bodies). The text is the record key's `pipeline` part
//! (blaze-tier key.rs `OptimizedKeyIn::pipeline` holds [`Pipeline::name`], and the text's
//! digest goes with it through [`Pipeline::passes`]).

/// A named pipeline: the key holds its name, LLVM runs its text.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Pipeline {
    /// `default<O2>` without interprocedural inference over offered bodies.
    BlazeO2,
}

impl Pipeline {
    /// The pass pipeline text `LLVMRunPasses` parses. Empty while [`BLAZE_O2_PASSES`] is
    /// (see it): `compile` refuses an empty pipeline by name (`LlvmError::NoPipeline`), never
    /// running another one in its place.
    pub const fn passes(self) -> &'static str {
        match self {
            Pipeline::BlazeO2 => BLAZE_O2_PASSES,
        }
    }

    /// The name the record key holds.
    pub const fn name(self) -> &'static str {
        match self {
            Pipeline::BlazeO2 => "blaze-O2",
        }
    }
}

/// THE PASS LIST OF `blaze-O2`: NOT YET WRITTEN, ON PURPOSE.
///
/// Why it is empty (D1's writer, 2026-11-16): the brief allows the text from exactly two
/// sources, and neither is on this machine or in the tree:
/// 1. `opt -print-pipeline-passes` output for `default<O2>` of the pinned LLVM (23.1.4,
///    indexable-inc/llvm `e152d9fa2bc2`) kept as text in the tree or the pinned LLVM's docs:
///    no such text exists (searched: crates/, docs/, vendor/, the notes; the installed prefix
///    `~/.cache/blaze-llvm` has headers and libraries only, no `share/` or docs);
/// 2. `llvm/lib/Passes/PassBuilderPipelines.cpp` of the pinned source: not present (the
///    prefix carries no source; /Volumes/Projects/ref-rust-1115/src/llvm-project is an empty
///    submodule; /Volumes/Projects/llvm20-private is LLVM 20, not the pin).
/// The prefix has no `opt` either (its `bin/` holds llvm-config, llvm-dwarfdump and
/// llvm-pdbutil), and a list from memory is refused by the brief. The person who fills it gets
/// the pinned LLVM's own text by either: `opt -passes='default<O2>' -print-pipeline-passes
/// -disable-output < /dev/null` from an LLVM built at the pin; or, in-process with the linked
/// LLVM, a few lines of C++ beside wrappers/passes.cpp's: `PassBuilder PB(TM);
/// ModulePassManager MPM = PB.buildPerModuleDefaultPipeline(OptimizationLevel::O2);
/// MPM.printPipeline(OS, MapClassName2PassName)` (what `-print-pipeline-passes` does). Then
/// delete the passes named in this module's documentation and paste the result here as one line.
pub const BLAZE_O2_PASSES: &str = "";
