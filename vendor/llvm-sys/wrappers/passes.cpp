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
 */
#include <llvm-c/Error.h>
#include <llvm-c/TargetMachine.h>
#include <llvm-c/Types.h>
#include <llvm/ADT/IntrusiveRefCntPtr.h>
#include <llvm/ADT/StringMap.h>
#include <llvm/ADT/StringRef.h>
#include <llvm/IR/DiagnosticHandler.h>
#include <llvm/IR/DiagnosticInfo.h>
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

#include <cstdint>
#include <memory>
#include <optional>

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

} // namespace

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

/* Runs `Manager` over `Mod` with `Kept`'s analyses, counting the mismatches of a profile used
 * into `Passes`, and forgets the analyses after. */
void runCounted(LLVMBlazeOpaquePasses &Passes, Pipelines &Kept, ModulePassManager &Manager,
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
  Manager.run(Mod, Kept.Modules);
  Kept.forget();
  std::unique_ptr<DiagnosticHandler> Taken = Context.getDiagnosticHandler();
  Passes.Mismatched += Counting->Count;
  Context.setDiagnosticHandler(std::move(Counting->Outer));
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

} // extern "C"
