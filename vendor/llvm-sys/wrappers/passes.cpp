/* blaze: LLVM's new pass manager kept per thread and target machine.
 *
 * LLVMRunPasses builds, on every call, a PassBuilder, four analysis managers with every
 * analysis registered in them, the standard instrumentation (whose timer groups are looked up
 * under a process-wide lock) and the pass pipeline parsed from its text. blaze runs passes
 * over one small module per function, so that was paid once per function, on every core at
 * once. Here it is built once per target machine and context, and a module's run costs what
 * its passes do. It runs the passes LLVMRunPasses would, with the same tuning options and
 * instrumentation.
 *
 * An object is used by one thread at a time, and outlives no target machine or context it was
 * used with: blaze keeps one beside each target machine of a thread, and drops it first.
 *
 * LLVMBlazeRunSanitizers runs the sanitizers' instrumentation with the same analysis
 * managers, as rustc runs it at the end of its pipeline.
 *
 * LLVMBlazeCreatePgoPasses keeps passes whose pipelines instrument for PGO or use a profile, as
 * rustc's LLVMRustOptimize builds its PassBuilder with PGOOptions for -Cprofile-generate and
 * -Cprofile-use. A profile is read from memory, never from a file.
 *
 * LLVMBlazeReport* keep what blaze's report (the `blaze::vecreport` lint) reads of a module's
 * optimized code: the loop vectorizer's remarks of why it left a loop scalar, caught by a
 * diagnostic handler while the passes run, and the branches out of a loop to code that never
 * returns (a check whose panic path survived), found once the passes are done. Each finding
 * is placed by the line of its debug location, which blaze makes the statement of the code.
 */
#include <llvm-c/Error.h>
#include <llvm-c/TargetMachine.h>
#include <llvm-c/Types.h>
#include <llvm/ADT/IntrusiveRefCntPtr.h>
#include <llvm/ADT/StringMap.h>
#include <llvm/ADT/StringRef.h>
#include <llvm/Analysis/BlockFrequencyInfo.h>
#include <llvm/Analysis/BranchProbabilityInfo.h>
#include <llvm/Analysis/LoopInfo.h>
#include <llvm/Analysis/ProfileSummaryInfo.h>
#include <llvm/IR/CFG.h>
#include <llvm/IR/DebugInfoMetadata.h>
#include <llvm/IR/DiagnosticHandler.h>
#include <llvm/IR/DiagnosticInfo.h>
#include <llvm/IR/Dominators.h>
#include <llvm/IR/Instructions.h>
#include <llvm/IR/IntrinsicInst.h>
#include <llvm/IR/LLVMContext.h>
#include <llvm/IR/Module.h>
#include <llvm/IR/PassManager.h>
#include <llvm/MC/MCSubtargetInfo.h>
#include <llvm/Passes/PassBuilder.h>
#include <llvm/Passes/StandardInstrumentations.h>
#include <llvm/ProfileData/InstrProf.h>
#include <llvm/ProfileData/InstrProfWriter.h>
#include <llvm/Support/Error.h>
#include <llvm/Support/MemoryBuffer.h>
#include <llvm/Support/PGOOptions.h>
#include <llvm/Support/VirtualFileSystem.h>
#include <llvm/Target/TargetMachine.h>
#include <llvm/Transforms/Instrumentation/AddressSanitizer.h>
#include <llvm/Transforms/Instrumentation/MemorySanitizer.h>
#include <llvm/Transforms/Instrumentation/SanitizerCoverage.h>
#include <llvm/Transforms/Instrumentation/ThreadSanitizer.h>
#include <llvm/Transforms/Utils/Instrumentation.h>

#include <algorithm>
#include <cstdint>
#include <map>
#include <memory>
#include <optional>
#include <string>
#include <vector>

using namespace llvm;

namespace {

PipelineTuningOptions tuning(bool SLPVectorization) {
  PipelineTuningOptions Options;
  Options.SLPVectorization = SLPVectorization;
  return Options;
}

/* What LLVMRunPasses makes for one call, made once for one context. Members are built in this
 * order and destroyed in the reverse, as LLVMRunPasses's locals are. */
struct Pipelines {
  Pipelines(TargetMachine *Machine, bool SLPVectorization, LLVMContext &Context,
            const std::optional<PGOOptions> &Pgo, IntrusiveRefCntPtr<vfs::FileSystem> FS)
      : Context(Context), Tuning(tuning(SLPVectorization)),
        Builder(Machine, Tuning, Pgo, &Callbacks, FS ? FS : vfs::getRealFileSystem()),
        Instrumentation(Context, /*DebugLogging=*/false, /*VerifyEach=*/false) {
    Builder.registerLoopAnalyses(Loops);
    Builder.registerFunctionAnalyses(Functions);
    Builder.registerCGSCCAnalyses(Components);
    Builder.registerModuleAnalyses(Modules);
    Builder.crossRegisterProxies(Loops, Functions, Components, Modules);
    Instrumentation.registerCallbacks(Callbacks, &Modules);
  }

  /* Forgets every analysis of the module that ran: the next is another module. The analyses
   * registered stay. */
  void forget() {
    Modules.clear();
    Components.clear();
    Functions.clear();
    Loops.clear();
  }

  LLVMContext &Context;
  PipelineTuningOptions Tuning;
  PassInstrumentationCallbacks Callbacks;
  PassBuilder Builder;
  LoopAnalysisManager Loops;
  FunctionAnalysisManager Functions;
  CGSCCAnalysisManager Components;
  ModuleAnalysisManager Modules;
  StandardInstrumentations Instrumentation;
  /* Each pipeline text as parsed, for the pipelines a run leaves as it found them. */
  StringMap<std::unique_ptr<ModulePassManager>> Parsed;
};

/* Whether a pipeline parsed once can run over every module. Two inliners can't:
 * - The CGSCC inliner run on its own makes an inline advisor of the first module it runs on
 *   and keeps it (InlinerPass::OwnedAdvisor), so a second module is optimized by what the
 *   first left.
 * - The module inliner the default, LTO and ThinLTO pipelines wrap it in runs once: its run
 *   moves its CGSCC pipeline and the module passes after it into its own manager
 *   (ModuleInlinerWrapperPass::run), so a second run is handed null passes and crashes in
 *   PassInstrumentation::runBeforePass.
 * A pipeline naming either is parsed again for each module. */
bool reusable(StringRef Text) {
  return !Text.contains("inline") && !Text.contains("default<") && !Text.contains("lto");
}

/* A finding of the report as the passes keep it until LLVMBlazeReportRows hands it out. */
struct Held {
  uint8_t Kind;
  uint32_t Line;
  uint32_t LoopLine;
  std::optional<uint64_t> Count;
  std::string Name;
  std::string Message;
};

} // namespace

/* A finding of the report. Declared field for field in src/transforms/pass_builder.rs. */
struct LLVMBlazeReportRow {
  /* LLVMBlazeFoundKept, LLVMBlazeFoundMissed or LLVMBlazeFoundAnalysis. */
  uint8_t Kind;
  /* LLVMBlazeHeatUnprofiled, LLVMBlazeHeatUncounted, LLVMBlazeHeatCold or
   * LLVMBlazeHeatHot. */
  uint8_t Heat;
  /* The line of the code's debug location, through any inlining to the function's own; 0 for
   * code of no line. Kept: of the call that never returns. A remark: of its location. */
  uint32_t Line;
  /* The line of the loop's start; 0 where it is not known. */
  uint32_t LoopLine;
  /* The loop header's profile count, for Cold and Hot. */
  uint64_t Count;
  /* Kept: the name of the function called (empty for a call through a pointer). A remark: its
   * name (`CantVectorizeCall`). */
  const char *Name;
  size_t NameLength;
  /* A remark's message; empty for Kept. */
  const char *Message;
  size_t MessageLength;
};

enum : uint8_t {
  LLVMBlazeFoundKept = 1,
  LLVMBlazeFoundMissed = 2,
  LLVMBlazeFoundAnalysis = 3,
};

enum : uint8_t {
  /* The module has no profile summary: built without a profile. */
  LLVMBlazeHeatUnprofiled = 0,
  /* The module has a profile, and no count reaches this loop (it is gone, or uncounted). */
  LLVMBlazeHeatUncounted = 1,
  LLVMBlazeHeatCold = 2,
  LLVMBlazeHeatHot = 3,
};

struct LLVMBlazeOpaquePasses {
  TargetMachine *Machine;
  std::unique_ptr<Pipelines> Vectorizing;
  std::unique_ptr<Pipelines> Plain;
  /* What every pipeline is built with for PGO: none for LLVMBlazeCreatePasses's. */
  std::optional<PGOOptions> Pgo = std::nullopt;
  /* Where a profile used is read from: the profile alone, in memory. */
  IntrusiveRefCntPtr<vfs::FileSystem> FS = nullptr;
  /* The start of the warning PGOInstrumentationUse gives a function whose hash is not the
   * profile's record of its name, and how many it gave since LLVMBlazeTakeMismatched. */
  std::string MismatchText = "";
  uint32_t Mismatched = 0;
  /* How many functions a profile used gave counts to (an entry count they did not have
   * before the run) since LLVMBlazeTakeApplied. */
  uint32_t Applied = 0;
  /* Whether the runs catch the loop vectorizer's remarks into Report (LLVMBlazeReportStart). */
  bool Reporting = false;
  /* The findings since LLVMBlazeReportStart, in the order LLVM made them. */
  std::vector<Held> Report = {};
  /* The vectorizer's reasons since its last "loop not vectorized": they are of the loop it
   * names next, whose line they are given then. */
  std::vector<size_t> Pending = {};
  /* Report as LLVMBlazeReportRows hands it out, pointing into it. */
  std::vector<LLVMBlazeReportRow> Rows = {};
};

/* What LLVMBlazeCreatePgoPasses builds pipelines for: rustc's -Cprofile-generate (Generate)
 * or -Cprofile-use (Use). Declared field for field in src/transforms/pass_builder.rs. */
struct LLVMBlazePgo {
  uint8_t Action;
  /* Use: the indexed profile, Length bytes followed by a NUL, live as long as the passes. */
  const char *Profile;
  size_t Length;
};

enum : uint8_t {
  LLVMBlazePgoGenerate = 1,
  LLVMBlazePgoUse = 2,
};

/* A function's counts for LLVMBlazeWriteProfile, as llvm-profdata merges them. Declared field
 * for field in src/transforms/pass_builder.rs. */
struct LLVMBlazeProfileRecord {
  const char *Name;
  size_t NameLength;
  uint64_t Hash;
  const uint64_t *Counts;
  size_t CountsLength;
};

/* What LLVMBlazeRunSanitizers instruments for: rustc's -Zsanitizer, -Zsanitizer-recover and
 * -Zsanitizer-memory-track-origins, and SanitizerCoverage as -Cllvm-args=-sanitizer-coverage-*
 * sets it. Declared field for field in src/transforms/pass_builder.rs; a 0 is false. */
struct LLVMBlazeSanitizers {
  uint8_t Address;
  uint8_t AddressRecover;
  uint8_t Thread;
  uint8_t Memory;
  uint8_t MemoryRecover;
  /* -sanitizer-coverage-level: 1 the entry block, 2 every block, 3 and critical edges, 4 and
   * indirect calls. Any other: no SanitizerCoverage, as LLVM takes it. */
  uint8_t CoverageLevel;
  /* The LLVMBlazeCoverage* bits. */
  uint16_t CoverageFlags;
  /* 0, 1 or 2. */
  int32_t MemoryTrackOrigins;
};

/* SanitizerCoverage's -sanitizer-coverage-* switches, one bit each. The same values are
 * blaze_codegen::settings::Coverage's. */
enum : uint16_t {
  LLVMBlazeCoverageTracePC = 1 << 0,
  LLVMBlazeCoverageTracePCGuard = 1 << 1,
  LLVMBlazeCoverageInline8bitCounters = 1 << 2,
  LLVMBlazeCoverageInlineBoolFlag = 1 << 3,
  LLVMBlazeCoveragePCTable = 1 << 4,
  LLVMBlazeCoverageTraceCmp = 1 << 5,
  LLVMBlazeCoverageTraceDiv = 1 << 6,
  LLVMBlazeCoverageTraceGep = 1 << 7,
  LLVMBlazeCoverageTraceLoads = 1 << 8,
  LLVMBlazeCoverageTraceStores = 1 << 9,
  LLVMBlazeCoverageNoPrune = 1 << 10,
  LLVMBlazeCoverageStackDepth = 1 << 11,
  LLVMBlazeCoverageTracePCEntryExit = 1 << 12,
  LLVMBlazeCoverageControlFlow = 1 << 13,
};

namespace {

/* SanitizerCoverage's options as its -sanitizer-coverage-* switches give them
 * (SanitizerCoverage.cpp, getOptions and OverrideFromCL): here per module rather than per
 * process, which a build of units with other settings needs. The pass still ORs in the
 * process's switches, which blaze leaves unset. */
SanitizerCoverageOptions coverage(const LLVMBlazeSanitizers &Wanted) {
  SanitizerCoverageOptions Options;
  switch (Wanted.CoverageLevel) {
  case 1:
    Options.CoverageType = SanitizerCoverageOptions::SCK_Function;
    break;
  case 2:
    Options.CoverageType = SanitizerCoverageOptions::SCK_BB;
    break;
  case 3:
    Options.CoverageType = SanitizerCoverageOptions::SCK_Edge;
    break;
  case 4:
    Options.CoverageType = SanitizerCoverageOptions::SCK_Edge;
    Options.IndirectCalls = true;
    break;
  default:
    break;
  }
  uint16_t Flags = Wanted.CoverageFlags;
  Options.TracePC = Flags & LLVMBlazeCoverageTracePC;
  Options.TracePCGuard = Flags & LLVMBlazeCoverageTracePCGuard;
  Options.Inline8bitCounters = Flags & LLVMBlazeCoverageInline8bitCounters;
  Options.InlineBoolFlag = Flags & LLVMBlazeCoverageInlineBoolFlag;
  Options.PCTable = Flags & LLVMBlazeCoveragePCTable;
  Options.TraceCmp = Flags & LLVMBlazeCoverageTraceCmp;
  Options.TraceDiv = Flags & LLVMBlazeCoverageTraceDiv;
  Options.TraceGep = Flags & LLVMBlazeCoverageTraceGep;
  Options.TraceLoads = Flags & LLVMBlazeCoverageTraceLoads;
  Options.TraceStores = Flags & LLVMBlazeCoverageTraceStores;
  Options.NoPrune = Flags & LLVMBlazeCoverageNoPrune;
  Options.StackDepth = Flags & LLVMBlazeCoverageStackDepth;
  Options.TracePCEntryExit = Flags & LLVMBlazeCoverageTracePCEntryExit;
  Options.CollectControlFlow = Flags & LLVMBlazeCoverageControlFlow;
  return Options;
}

bool covers(const LLVMBlazeSanitizers &Wanted) {
  return Wanted.CoverageLevel >= 1 && Wanted.CoverageLevel <= 4;
}

/* The name the profile used has in the passes' file system, which holds it alone. */
constexpr const char *ProfileName = "blaze.profdata";

/* Where an instrumented program writes its profile unless LLVM_PROFILE_FILE says otherwise:
 * rustc's for -Cprofile-generate with no directory. The same in every function, so that the
 * code is one whatever the directory; the link names the binary's. */
constexpr const char *RawProfileName = "default_%m.profraw";

/* Counts the functions PGOInstrumentationUse found a record of under their name with another
 * hash, which it warns of one function at a time, and hands every other diagnostic to the
 * handler it stands in for. */
struct Mismatches final : DiagnosticHandler {
  Mismatches(std::unique_ptr<DiagnosticHandler> Outer, StringRef Text)
      : Outer(std::move(Outer)), Text(Text) {}

  bool handleDiagnostics(const DiagnosticInfo &DI) override {
    if (const auto *Profile = dyn_cast<DiagnosticInfoPGOProfile>(&DI)) {
      if (Profile->getSeverity() == DS_Warning &&
          StringRef(Profile->getMsg().str()).starts_with(Text)) {
        ++Count;
        return true;
      }
    }
    return Outer->handleDiagnostics(DI);
  }
  bool isAnalysisRemarkEnabled(StringRef Pass) const override {
    return Outer->isAnalysisRemarkEnabled(Pass);
  }
  bool isMissedOptRemarkEnabled(StringRef Pass) const override {
    return Outer->isMissedOptRemarkEnabled(Pass);
  }
  bool isPassedOptRemarkEnabled(StringRef Pass) const override {
    return Outer->isPassedOptRemarkEnabled(Pass);
  }
  bool isAnyRemarkEnabled() const override { return Outer->isAnyRemarkEnabled(); }

  std::unique_ptr<DiagnosticHandler> Outer;
  StringRef Text;
  uint32_t Count = 0;
};

/* The pass name of the loop vectorizer's remarks (LoopVectorize.cpp's LV_NAME). */
constexpr const char *Vectorizer = "loop-vectorize";

/* Catches the loop vectorizer's remarks of a loop it left scalar (missed) and of why
 * (analysis) into the report, and hands every other diagnostic to the handler it stands in
 * for. A remark only describes: asking for them changes no code (LLVM's remarks are
 * observers, as -Rpass-missed is in clang). */
struct Remarks final : DiagnosticHandler {
  Remarks(std::unique_ptr<DiagnosticHandler> Outer, LLVMBlazeOpaquePasses &Passes)
      : Outer(std::move(Outer)), Passes(Passes) {}

  bool handleDiagnostics(const DiagnosticInfo &DI) override {
    const auto *Remark = dyn_cast<DiagnosticInfoIROptimization>(&DI);
    if (!Remark || Remark->getPassName() != Vectorizer)
      return Outer->handleDiagnostics(DI);
    /* Only the function's own (or its body, before it is built into its wrapper): an import's
     * body, available_externally until elim-avail-extern, is another function's report. */
    StringRef Of = Remark->getFunction().getName();
    if (Of != "blaze.own" && Of != "blaze.inner")
      return true;
    uint8_t Kind = 0;
    switch (Remark->getKind()) {
    case DK_OptimizationRemarkMissed:
      Kind = LLVMBlazeFoundMissed;
      break;
    case DK_OptimizationRemarkAnalysis:
    case DK_OptimizationRemarkAnalysisFPCommute:
    case DK_OptimizationRemarkAnalysisAliasing:
      Kind = LLVMBlazeFoundAnalysis;
      break;
    default:
      return Outer->handleDiagnostics(DI);
    }
    uint32_t Line = Remark->isLocationAvailable() ? Remark->getLocation().getLine() : 0;
    Held Row{Kind, Line, 0, std::nullopt, Remark->getRemarkName().str(), Remark->getMsg()};
    if (Kind == LLVMBlazeFoundMissed) {
      Row.LoopLine = Line;
      for (size_t At : Passes.Pending)
        Passes.Report[At].LoopLine = Line;
      Passes.Pending.clear();
    } else {
      Passes.Pending.push_back(Passes.Report.size());
    }
    Passes.Report.push_back(std::move(Row));
    return true;
  }
  bool isAnalysisRemarkEnabled(StringRef Pass) const override {
    return Pass == Vectorizer || Outer->isAnalysisRemarkEnabled(Pass);
  }
  bool isMissedOptRemarkEnabled(StringRef Pass) const override {
    return Pass == Vectorizer || Outer->isMissedOptRemarkEnabled(Pass);
  }
  bool isPassedOptRemarkEnabled(StringRef Pass) const override {
    return Outer->isPassedOptRemarkEnabled(Pass);
  }
  bool isAnyRemarkEnabled() const override { return true; }

  std::unique_ptr<DiagnosticHandler> Outer;
  LLVMBlazeOpaquePasses &Passes;
};

/* Runs `Manager` over `Mod` with `Kept`'s analyses, counting the mismatches of a profile used
 * into `Passes`, and forgets the analyses after. */
void runMeasured(LLVMBlazeOpaquePasses &Passes, Pipelines &Kept, ModulePassManager &Manager,
                 Module &Mod) {
  bool Using = Passes.Pgo && Passes.Pgo->Action == PGOOptions::IRUse;
  if (!Using) {
    Manager.run(Mod, Kept.Modules);
    Kept.forget();
    return;
  }
  LLVMContext &Context = Mod.getContext();
  auto Counter = std::make_unique<Mismatches>(Context.getDiagnosticHandler(), Passes.MismatchText);
  Mismatches *Counting = Counter.get();
  Context.setDiagnosticHandler(std::move(Counter));
  /* The functions with no entry count yet, by name (the run may delete one): those that
   * have one after it were given counts by the profile. A module read back with its counts
   * is not counted again. */
  std::vector<std::string> Uncounted;
  for (Function &F : Mod)
    if (!F.isDeclaration() && !F.getEntryCount())
      Uncounted.push_back(F.getName().str());
  Manager.run(Mod, Kept.Modules);
  Kept.forget();
  std::unique_ptr<DiagnosticHandler> Taken = Context.getDiagnosticHandler();
  Passes.Mismatched += Counting->Count;
  for (const std::string &Name : Uncounted)
    if (const Function *F = Mod.getFunction(Name); F && !F->isDeclaration() && F->getEntryCount())
      ++Passes.Applied;
  Context.setDiagnosticHandler(std::move(Counting->Outer));
}

/* runMeasured, with the vectorizer's remarks caught into the report while `Passes` is
 * reporting (LLVMBlazeReportStart). */
void runCounted(LLVMBlazeOpaquePasses &Passes, Pipelines &Kept, ModulePassManager &Manager,
                Module &Mod) {
  if (!Passes.Reporting) {
    runMeasured(Passes, Kept, Manager, Mod);
    return;
  }
  LLVMContext &Context = Mod.getContext();
  auto Catcher = std::make_unique<Remarks>(Context.getDiagnosticHandler(), Passes);
  Remarks *Catching = Catcher.get();
  Context.setDiagnosticHandler(std::move(Catcher));
  runMeasured(Passes, Kept, Manager, Mod);
  std::unique_ptr<DiagnosticHandler> Taken = Context.getDiagnosticHandler();
  Context.setDiagnosticHandler(std::move(Catching->Outer));
}

/* The line of `Location` in the function's own code: through the calls it was inlined at, to
 * the outermost. 0 for none. */
uint32_t lineOf(const DebugLoc &Location) {
  const DILocation *At = Location.get();
  if (!At)
    return 0;
  /* Bounded by the depth of inlining, which the IR holds finitely; the fuel names the bound. */
  for (unsigned Fuel = 0; Fuel < 4096 && At->getInlinedAt(); ++Fuel)
    At = At->getInlinedAt();
  return At->getLine();
}

/* The first instruction of `Block` with a debug location's line, 0 for none. */
uint32_t firstLine(const BasicBlock &Block) {
  for (const Instruction &I : Block)
    if (uint32_t Line = lineOf(I.getDebugLoc()))
      return Line;
  return 0;
}

/* Whether `I` is a call that says nothing of the code: a debug or lifetime marker. */
bool isMarker(const Instruction &I) {
  if (const auto *Intrinsic = dyn_cast<IntrinsicInst>(&I))
    return Intrinsic->isAssumeLikeIntrinsic() || Intrinsic->isLifetimeStartOrEnd();
  return false;
}

/* The call that starts the panic path at `Exit`, a block a loop branches out to: code that
 * goes on, block by block with no choice, to `unreachable` (or is an invoke whose normal
 * destination does). The call is the first of the path (the panic function: `llvm.trap` only
 * when the path calls nothing else). Null when `Exit` can return. */
const CallBase *panicPath(const BasicBlock *Exit) {
  const CallBase *First = nullptr;
  const BasicBlock *At = Exit;
  /* A path of more blocks than this is not a panic path blaze's lowering makes. */
  for (unsigned Fuel = 0; Fuel < 16 && At; ++Fuel) {
    for (const Instruction &I : *At) {
      const auto *Call = dyn_cast<CallBase>(&I);
      if (!Call || isMarker(I))
        continue;
      bool Trap = isa<IntrinsicInst>(Call) &&
                  cast<IntrinsicInst>(Call)->getIntrinsicID() == Intrinsic::trap;
      if (!First || (isa<IntrinsicInst>(First) && !Trap))
        First = Call;
    }
    const Instruction *End = At->getTerminator();
    if (!End)
      return nullptr;
    if (isa<UnreachableInst>(End))
      return First;
    if (const auto *Invoke = dyn_cast<InvokeInst>(End)) {
      const Instruction *Then = Invoke->getNormalDest()->getTerminator();
      return Then && isa<UnreachableInst>(Then) ? Invoke : nullptr;
    }
    At = At->getSingleSuccessor();
  }
  return nullptr;
}

/* The pipelines of `Passes` for `SLPVectorization`, made for the context of `Mod` the first
 * time; an error for a module of another context than they were made for. */
Expected<Pipelines *> slot(LLVMBlazeOpaquePasses &Passes, bool SLPVectorization, Module &Mod) {
  std::unique_ptr<Pipelines> &Slot = SLPVectorization ? Passes.Vectorizing : Passes.Plain;
  if (!Slot) {
    Slot = std::make_unique<Pipelines>(Passes.Machine, SLPVectorization, Mod.getContext(),
                                       Passes.Pgo, Passes.FS);
  } else if (&Slot->Context != &Mod.getContext()) {
    return make_error<StringError>(
        "blaze: passes kept for one LLVM context were given a module of another",
        inconvertibleErrorCode());
  }
  return Slot.get();
}

} // namespace

extern "C" {

LLVMBlazeOpaquePasses *LLVMBlazeCreatePasses(LLVMTargetMachineRef Machine) {
  return new LLVMBlazeOpaquePasses{reinterpret_cast<TargetMachine *>(Machine), nullptr,
                                   nullptr};
}

void LLVMBlazeDisposePasses(LLVMBlazeOpaquePasses *Passes) { delete Passes; }

/* Whether `Machine`'s target has a feature named `Name` (`Length` bytes), as rustc's
 * LLVMRustHasFeature asks: LLVM told a name it lacks only prints a warning and ignores it. */
LLVMBool LLVMBlazeHasFeature(LLVMTargetMachineRef Machine, const char *Name, size_t Length) {
  const MCSubtargetInfo &Info = reinterpret_cast<TargetMachine *>(Machine)->getMCSubtargetInfo();
  StringRef Wanted(Name, Length);
  for (const SubtargetFeatureKV &Feature : Info.getAllProcessorFeatures())
    if (Wanted == Feature.key())
      return 1;
  return 0;
}

/* Whether `Machine`'s target knows the CPU `Name` (`Length` bytes), aliases included (AArch64's
 * `isCPUStringValid` resolves `apple-m1` to `apple-a14`): LLVM told a CPU it does not know only
 * prints a warning and compiles for none. */
LLVMBool LLVMBlazeHasCpu(LLVMTargetMachineRef Machine, const char *Name, size_t Length) {
  const MCSubtargetInfo &Info = reinterpret_cast<TargetMachine *>(Machine)->getMCSubtargetInfo();
  return Info.isCPUStringValid(StringRef(Name, Length)) ? 1 : 0;
}

/* Runs the pipeline `Text` (`Length` bytes) over `M`, as LLVMRunPasses does with options whose
 * only change is SLP vectorization. Every module given to one object is of one context. */
LLVMErrorRef LLVMBlazeRunPasses(LLVMBlazeOpaquePasses *Passes, LLVMModuleRef M,
                                const char *Text, size_t Length,
                                LLVMBool SLPVectorization) {
  Module &Mod = *unwrap(M);
  Expected<Pipelines *> Slot = slot(*Passes, SLPVectorization != 0, Mod);
  if (!Slot)
    return wrap(Slot.takeError());
  Pipelines &Kept = **Slot;
  StringRef Pipeline(Text, Length);
  std::unique_ptr<ModulePassManager> Once;
  ModulePassManager *Manager = nullptr;
  if (reusable(Pipeline)) {
    auto Found = Kept.Parsed.find(Pipeline);
    if (Found != Kept.Parsed.end())
      Manager = Found->second.get();
  }
  if (!Manager) {
    auto Parsed = std::make_unique<ModulePassManager>();
    if (Error Err = Kept.Builder.parsePassPipeline(*Parsed, Pipeline))
      return wrap(std::move(Err));
    Manager = Parsed.get();
    if (reusable(Pipeline))
      Kept.Parsed.try_emplace(Pipeline, std::move(Parsed));
    else
      Once = std::move(Parsed);
  }
  runCounted(*Passes, Kept, *Manager, Mod);
  return nullptr;
}

/* Instruments `M` for `Wanted` (not null): SanitizerCoverage, then MemorySanitizer,
 * ThreadSanitizer and AddressSanitizer, in clang's order (BackendUtil.cpp, addSanitizers), so
 * that coverage traces none of the sanitizers' own checks; each with the options rustc's
 * LLVMRustOptimize gives it. Only functions with the attribute `sanitize_address`,
 * `sanitize_thread` or `sanitize_memory` get that sanitizer's checks. Every module given to
 * one object is of one context, as for LLVMBlazeRunPasses. */
LLVMErrorRef LLVMBlazeRunSanitizers(LLVMBlazeOpaquePasses *Passes, LLVMModuleRef M,
                                    const LLVMBlazeSanitizers *Wanted) {
  Module &Mod = *unwrap(M);
  Expected<Pipelines *> Found = slot(*Passes, false, Mod);
  if (!Found)
    return wrap(Found.takeError());
  Pipelines *Slot = *Found;
  ModulePassManager Manager;
  if (covers(*Wanted)) {
    Manager.addPass(SanitizerCoveragePass(coverage(*Wanted)));
  }
  if (Wanted->Memory) {
    MemorySanitizerOptions Options(Wanted->MemoryTrackOrigins, Wanted->MemoryRecover != 0,
                                   /*Kernel=*/false, /*EagerChecks=*/true);
    Manager.addPass(MemorySanitizerPass(Options));
  }
  if (Wanted->Thread) {
    Manager.addPass(ModuleThreadSanitizerPass());
    Manager.addPass(createModuleToFunctionPassAdaptor(ThreadSanitizerPass()));
  }
  if (Wanted->Address) {
    AddressSanitizerOptions Options;
    Options.CompileKernel = false;
    Options.Recover = Wanted->AddressRecover != 0;
    Options.UseAfterScope = true;
    Options.UseAfterReturn = AsanDetectStackUseAfterReturnMode::Runtime;
    /* Windows has no ODR indicators (https://reviews.llvm.org/D137227), as rustc says. */
    bool Odr = !Passes->Machine->getTargetTriple().isOSWindows();
    Manager.addPass(AddressSanitizerPass(Options, /*UseGlobalGC=*/true, Odr));
  }
  Manager.run(Mod, Slot->Modules);
  Slot->forget();
  return nullptr;
}

/* Passes whose pipelines are built with `Pgo` (not null, read during the call only), as
 * LLVMRustOptimize builds its PassBuilder: IRInstr writing `default_%m.profraw`, or IRUse of
 * the profile `Pgo->Profile`, which the passes read from memory and which outlives them. The
 * default pipelines named in LLVMBlazeRunPasses run PGO's passes where LLVM puts them (the
 * per-module and ThinLTO pre-link pipelines; not ThinLTO's post-link one); LLVMBlazeRunPgo
 * runs them alone. Null for an action that is neither. */
LLVMBlazeOpaquePasses *LLVMBlazeCreatePgoPasses(LLVMTargetMachineRef Machine,
                                                const LLVMBlazePgo *Pgo) {
  auto *Passes = new LLVMBlazeOpaquePasses{reinterpret_cast<TargetMachine *>(Machine), nullptr,
                                           nullptr};
  switch (Pgo->Action) {
  case LLVMBlazePgoGenerate:
    Passes->Pgo = PGOOptions(RawProfileName, "", "", "", PGOOptions::IRInstr,
                             PGOOptions::NoCSAction);
    break;
  case LLVMBlazePgoUse: {
    auto Files = makeIntrusiveRefCnt<vfs::InMemoryFileSystem>();
    Files->addFile(ProfileName, 0,
                   MemoryBuffer::getMemBuffer(StringRef(Pgo->Profile, Pgo->Length), ProfileName,
                                              /*RequiresNullTerminator=*/true));
    Passes->FS = Files;
    Passes->Pgo =
        PGOOptions(ProfileName, "", "", "", PGOOptions::IRUse, PGOOptions::NoCSAction);
    Passes->MismatchText = InstrProfError(instrprof_error::hash_mismatch).message();
    break;
  }
  default:
    delete Passes;
    return nullptr;
  }
  return Passes;
}

/* Runs PGO's passes alone over `M`, as LLVM runs them at -O0 (addPGOInstrPassesForO0): the
 * instrumentation and its lowering, or the profile's use. For passes LLVMBlazeCreatePgoPasses
 * made; `M` is of the context they were first run with. */
LLVMErrorRef LLVMBlazeRunPgo(LLVMBlazeOpaquePasses *Passes, LLVMModuleRef M) {
  if (!Passes->Pgo)
    return wrap(make_error<StringError>("blaze: PGO's passes of passes made without PGO",
                                        inconvertibleErrorCode()));
  Module &Mod = *unwrap(M);
  Expected<Pipelines *> Found = slot(*Passes, false, Mod);
  if (!Found)
    return wrap(Found.takeError());
  Pipelines &Kept = **Found;
  const PGOOptions &Pgo = *Passes->Pgo;
  ModulePassManager Manager;
  Kept.Builder.addPGOInstrPassesForO0(Manager, Pgo.Action == PGOOptions::IRInstr,
                                      /*IsCS=*/false, Pgo.AtomicCounterUpdate, Pgo.ProfileFile,
                                      /*ProfileRemappingFile=*/"");
  runCounted(*Passes, Kept, Manager, Mod);
  return nullptr;
}

/* How many functions the passes found a record of in the profile they use under the
 * function's name with another hash since this was last asked. */
uint32_t LLVMBlazeTakeMismatched(LLVMBlazeOpaquePasses *Passes) {
  uint32_t Mismatched = Passes->Mismatched;
  Passes->Mismatched = 0;
  return Mismatched;
}

/* How many functions the profile the passes use gave counts to since this was last asked. */
uint32_t LLVMBlazeTakeApplied(LLVMBlazeOpaquePasses *Passes) {
  uint32_t Applied = Passes->Applied;
  Passes->Applied = 0;
  return Applied;
}

/* The indexed profile of IR instrumentation with the `Count` records `Records`, as
 * llvm-profdata merge writes it, into `*Out`, which the caller disposes of. */
LLVMErrorRef LLVMBlazeWriteProfile(const LLVMBlazeProfileRecord *Records, size_t Count,
                                   LLVMMemoryBufferRef *Out) {
  InstrProfWriter Writer;
  if (Error Err = Writer.mergeProfileKind(InstrProfKind::IRInstrumentation))
    return wrap(std::move(Err));
  Error Failed = Error::success();
  for (size_t At = 0; At < Count; ++At) {
    const LLVMBlazeProfileRecord &Record = Records[At];
    std::vector<uint64_t> Counts(Record.Counts, Record.Counts + Record.CountsLength);
    NamedInstrProfRecord Named(StringRef(Record.Name, Record.NameLength), Record.Hash,
                               std::move(Counts));
    Writer.addRecord(std::move(Named), 1, [&Failed](Error Err) {
      Failed = joinErrors(std::move(Failed), std::move(Err));
    });
  }
  if (Failed)
    return wrap(std::move(Failed));
  *Out = wrap(Writer.writeBuffer().release());
  return nullptr;
}

/* Starts a report: what was kept is let go, and the runs from now on catch the loop
 * vectorizer's remarks until LLVMBlazeReportStop. */
void LLVMBlazeReportStart(LLVMBlazeOpaquePasses *Passes) {
  Passes->Report.clear();
  Passes->Pending.clear();
  Passes->Rows.clear();
  Passes->Reporting = true;
}

/* Ends the report: the runs from now on catch no remarks, and what was kept is let go. */
void LLVMBlazeReportStop(LLVMBlazeOpaquePasses *Passes) {
  Passes->Reporting = false;
  Passes->Report.clear();
  Passes->Pending.clear();
  Passes->Rows.clear();
}

/* Adds to the report what `Fn`'s optimized code still does inside its loops: each branch out
 * of a loop to a panic path (panicPath), once per loop and path, placed at the path's call,
 * with the innermost loop it leaves. Then gives every finding its loop's heat: the loop
 * header's profile count, hot or cold as the module's profile summary says (none for a
 * module built without a profile). The order is the function's blocks' and their
 * successors': deterministic for one module. */
void LLVMBlazeReportKept(LLVMBlazeOpaquePasses *Passes, LLVMValueRef Fn) {
  Function &F = *unwrap<Function>(Fn);
  if (F.isDeclaration())
    return;
  DominatorTree Dominators(F);
  LoopInfo Loops(Dominators);
  BranchProbabilityInfo Probabilities(F, Loops);
  BlockFrequencyInfo Frequencies(F, Probabilities, Loops);
  ProfileSummaryInfo Summary(*F.getParent());
  /* Each loop's header count, by both lines a loop is placed at: its start (Loop::getStartLoc,
   * as the vectorizer places its remarks) and its header's first. */
  std::map<uint32_t, std::optional<uint64_t>> Counts;
  for (const Loop *L : Loops.getLoopsInPreorder()) {
    std::optional<uint64_t> Count = Frequencies.getBlockProfileCount(L->getHeader());
    for (uint32_t Line : {lineOf(L->getStartLoc()), firstLine(*L->getHeader())})
      if (Line)
        Counts.try_emplace(Line, Count);
  }
  std::vector<std::pair<const BasicBlock *, const Loop *>> Seen;
  for (const BasicBlock &Block : F) {
    const Loop *L = Loops.getLoopFor(&Block);
    if (!L)
      continue;
    for (const BasicBlock *Exit : successors(&Block)) {
      if (L->contains(Exit))
        continue;
      const CallBase *Call = panicPath(Exit);
      if (!Call)
        continue;
      std::pair<const BasicBlock *, const Loop *> Key{Exit, L};
      if (std::find(Seen.begin(), Seen.end(), Key) != Seen.end())
        continue;
      Seen.push_back(Key);
      uint32_t Line = lineOf(Call->getDebugLoc());
      if (!Line)
        Line = lineOf(Block.getTerminator()->getDebugLoc());
      uint32_t LoopLine = lineOf(L->getStartLoc());
      if (!LoopLine)
        LoopLine = firstLine(*L->getHeader());
      const Function *Callee = Call->getCalledFunction();
      std::string Name = Callee ? Callee->getName().str() : "";
      Passes->Report.push_back(Held{LLVMBlazeFoundKept, Line, LoopLine,
                                    Frequencies.getBlockProfileCount(L->getHeader()),
                                    std::move(Name), ""});
    }
  }
  for (Held &Row : Passes->Report) {
    if (Row.Kind == LLVMBlazeFoundKept)
      continue;
    auto Found = Counts.find(Row.LoopLine);
    Row.Count = Found == Counts.end() ? std::nullopt : Found->second;
  }
  bool Profiled = Summary.hasProfileSummary();
  Passes->Rows.clear();
  for (const Held &Row : Passes->Report) {
    uint8_t Heat = LLVMBlazeHeatUnprofiled;
    if (Profiled)
      Heat = !Row.Count                       ? LLVMBlazeHeatUncounted
             : Summary.isHotCount(*Row.Count) ? LLVMBlazeHeatHot
                                              : LLVMBlazeHeatCold;
    Passes->Rows.push_back(LLVMBlazeReportRow{
        Row.Kind, Heat, Row.Line, Row.LoopLine, Row.Count.value_or(0), Row.Name.data(),
        Row.Name.size(), Row.Message.data(), Row.Message.size()});
  }
}

/* The report's findings as LLVMBlazeReportKept last made them, into `*Out`: live until the
 * next LLVMBlazeReport* call on `Passes`. Returns how many. */
size_t LLVMBlazeReportRows(LLVMBlazeOpaquePasses *Passes, const LLVMBlazeReportRow **Out) {
  *Out = Passes->Rows.data();
  return Passes->Rows.size();
}

} // extern "C"
