//! The sanitizers of rustc's `-Zsanitizer` (address, thread, memory) and SanitizerCoverage on
//! the release path (`v2/requests/codegen.md` S2, `safety.md` F2).
//!
//! [`mark`] gives the function the attributes rustc gives it (`sanitize_attrs`), at lowering,
//! so every pipeline after sees them as rustc's do: what LLVM builds in is only what has the
//! same ones. [`instrument`] runs the instrumentation over the module once its passes are done,
//! as rustc's single module does at the end of its pipeline: what is instrumented is the code
//! that is written. [`super::emitted::read_covered`] reads back what SanitizerCoverage made
//! of it; the runtimes, the arrays of every function together and their initialization are
//! the link's (`v2/requests/link.md`).

#![forbid(unsafe_code)]

use blaze_codegen::settings::{Coverage, Sanitize, Sanitizers};
use inkwell::{
    ThreadLocalMode, attributes::AttributeLoc, context::Context,
    llvm_sys::transforms::pass_builder::LLVMBlazeSanitizers, module::Module, values::FunctionValue,
};

use super::{Machinery, Unhandled, attribute_kind};
use crate::backend::{Phase, Tally};

/// The relocation namespace of the function's own SanitizerCoverage arrays, by their index in
/// [`SECTIONS`]. Above the shape's namespaces and below `fold::BODY` and `fragment::INLINED`.
pub(crate) const COVERAGE: u32 = u32::MAX - 2;

/// The sections LLVM's SanitizerCoverage puts a function's arrays in, on ELF and Mach-O: its
/// 8-bit counters (`-sanitizer-coverage-inline-8bit-counters`), its flags
/// (`-sanitizer-coverage-inline-bool-flag`) and its guards (`-sanitizer-coverage-trace-pc-guard`).
pub(crate) const SECTIONS: [&str; 3] = ["__sancov_cntrs", "__sancov_bools", "__sancov_guards"];
/// The section of its PC table (`-sanitizer-coverage-pc-table`).
pub(crate) const PCS: &str = "__sancov_pcs";
/// The section of its control flow (`-sanitizer-coverage-control-flow`).
pub(crate) const CONTROL_FLOW: &str = "__sancov_cfs";

/// What SanitizerCoverage made for the function, beside its code.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Covered {
    /// How many bytes each of its arrays ([`SECTIONS`]) is, 0 for one it has not; each starts
    /// zeroed.
    pub(crate) arrays: [u32; 3],
    /// Its PC table: of each block it counts, in the order of its arrays, how far into the
    /// code the block starts and its flags (1 for the function's entry, else 0).
    pub(crate) pcs: Vec<(u32, u64)>,
}

/// The attributes of the sanitizers, in the order rustc gives them (`sanitize_attrs`).
const ATTRIBUTES: [(Sanitizers, &str); 3] = [
    (Sanitizers::ADDRESS, "sanitize_address"),
    (Sanitizers::MEMORY, "sanitize_memory"),
    (Sanitizers::THREAD, "sanitize_thread"),
];

/// Gives `function` the attribute of each sanitizer `sanitize` turns on that `off` (what its
/// `#[sanitize(.. = "off")]` turns off) leaves on, and `nosanitize_coverage` where `off` has
/// [`Sanitizers::COVERAGE`] and SanitizerCoverage is on. Nothing when `sanitize` is empty.
pub(super) fn mark<'ctx>(
    context: &'ctx Context,
    function: FunctionValue<'ctx>,
    sanitize: &Sanitize,
    off: Sanitizers,
) {
    let on = sanitize.sanitizers.without(off);
    for (sanitizer, name) in ATTRIBUTES {
        if on.contains(sanitizer) {
            let kind = attribute_kind(name);
            function.add_attribute(AttributeLoc::Function, context.create_enum_attribute(kind, 0));
        }
    }
    if sanitize.coverage.is_on() && off.contains(Sanitizers::COVERAGE) {
        let kind = attribute_kind("nosanitize_coverage");
        function.add_attribute(AttributeLoc::Function, context.create_enum_attribute(kind, 0));
    }
}

/// What the kept pass manager is given for `sanitize`, `None` when it asks for nothing.
fn wanted(sanitize: &Sanitize) -> Option<LLVMBlazeSanitizers> {
    if sanitize.is_empty() {
        return None;
    }
    let has = |set: Sanitizers, it| u8::from(set.contains(it));
    let (on, recover) = (sanitize.sanitizers, sanitize.recover);
    let coverage = sanitize.coverage.is_on();
    Some(LLVMBlazeSanitizers {
        Address: has(on, Sanitizers::ADDRESS),
        AddressRecover: has(recover, Sanitizers::ADDRESS),
        Thread: has(on, Sanitizers::THREAD),
        Memory: has(on, Sanitizers::MEMORY),
        MemoryRecover: has(recover, Sanitizers::MEMORY),
        CoverageLevel: if coverage { sanitize.coverage.level } else { 0 },
        CoverageFlags: if coverage { sanitize.coverage.flags } else { 0 },
        MemoryTrackOrigins: i32::from(sanitize.track_origins),
    })
}

/// Instruments `module` for `sanitize` through the passes kept for `machinery`: only its
/// functions [`mark`] marked get a sanitizer's checks. Nothing when `sanitize` is empty.
///
/// The thread-locals the instrumentation reads of the runtimes are made general-dynamic, the
/// one model the release path reads relocations of on every ELF target: LLVM declares them
/// initial-exec, which only an executable or a library loaded at its start can have.
pub(super) fn instrument(
    module: &Module<'_>,
    machinery: &Machinery,
    sanitize: &Sanitize,
) -> Result<(), Unhandled> {
    let Some(wanted) = wanted(sanitize) else { return Ok(()) };
    let triple = machinery.get_triple();
    let triple = triple.as_str().to_string_lossy();
    if wanted.Memory != 0 && !["-linux", "-freebsd", "-netbsd"].iter().any(|os| triple.contains(os)) {
        // alloc: the error message, once before the build stops
        return Err(Unhandled::Llvm(format!(
            "MemorySanitizer for {triple}, which LLVM has for Linux, FreeBSD and NetBSD only"
        )));
    }
    if wanted.CoverageLevel != 0 && triple.contains("-windows") {
        // alloc: the error message, once before the build stops
        return Err(Unhandled::Llvm(format!(
            "SanitizerCoverage for {triple}: the release path reads SanitizerCoverage's \
             sections of ELF and Mach-O objects only"
        )));
    }
    Tally::global().time(Phase::Optimize, || machinery.instrument(module, &wanted))?;
    for global in module.get_globals() {
        let name = global.get_name().to_bytes();
        let runtime = name.starts_with(b"__msan_") || name.starts_with(b"__sancov_");
        if runtime
            && global.is_declaration()
            && global.get_thread_local_mode().is_some_and(|it| it != ThreadLocalMode::GeneralDynamicTLSModel)
        {
            global.set_thread_local_mode(Some(ThreadLocalMode::GeneralDynamicTLSModel));
        }
    }
    Ok(())
}

impl Covered {
    /// How many blocks the arrays count, by the first array the function has.
    pub(crate) fn blocks(&self) -> u32 {
        let [counters, flags, guards] = self.arrays;
        [counters, flags, guards / 4].into_iter().find(|it| *it != 0).unwrap_or(0)
    }
}

/// [`Coverage`]'s flags are the wrapper's (`vendor/llvm-sys/wrappers/passes.cpp`).
const _: () = {
    use inkwell::llvm_sys::transforms::pass_builder as llvm;
    assert!(Coverage::TRACE_PC == llvm::LLVMBlazeCoverageTracePC);
    assert!(Coverage::TRACE_PC_GUARD == llvm::LLVMBlazeCoverageTracePCGuard);
    assert!(Coverage::INLINE_8BIT_COUNTERS == llvm::LLVMBlazeCoverageInline8bitCounters);
    assert!(Coverage::INLINE_BOOL_FLAG == llvm::LLVMBlazeCoverageInlineBoolFlag);
    assert!(Coverage::PC_TABLE == llvm::LLVMBlazeCoveragePCTable);
    assert!(Coverage::TRACE_CMP == llvm::LLVMBlazeCoverageTraceCmp);
    assert!(Coverage::TRACE_DIV == llvm::LLVMBlazeCoverageTraceDiv);
    assert!(Coverage::TRACE_GEP == llvm::LLVMBlazeCoverageTraceGep);
    assert!(Coverage::TRACE_LOADS == llvm::LLVMBlazeCoverageTraceLoads);
    assert!(Coverage::TRACE_STORES == llvm::LLVMBlazeCoverageTraceStores);
    assert!(Coverage::NO_PRUNE == llvm::LLVMBlazeCoverageNoPrune);
    assert!(Coverage::STACK_DEPTH == llvm::LLVMBlazeCoverageStackDepth);
    assert!(Coverage::TRACE_PC_ENTRY_EXIT == llvm::LLVMBlazeCoverageTracePCEntryExit);
    assert!(Coverage::CONTROL_FLOW == llvm::LLVMBlazeCoverageControlFlow);
};

#[cfg(test)]
mod tests {
    use inkwell::{memory_buffer::MemoryBuffer, targets::FileType};

    use super::*;
    use crate::{
        backend::{Optimization, Passes, Selection},
        llvm::{Machine, OWN},
    };

    /// A branch on a compare, a load and a store: something for each sanitizer to check and
    /// three blocks to count.
    const BODY: &str = r#"
define i32 @"blaze.own"(ptr %p, i32 %n) {
entry:
  %small = icmp slt i32 %n, 10
  br i1 %small, label %load, label %store
load:
  %v = load i32, ptr %p
  ret i32 %v
store:
  store i32 %n, ptr %p
  ret i32 0
}
"#;

    fn machinery() -> Machinery {
        let optimization = Optimization { passes: Passes::None, selection: Selection::Fast };
        Machinery::new(Machine::create(optimization).unwrap())
    }

    fn parsed<'ctx>(context: &'ctx Context, machinery: &Machinery) -> Module<'ctx> {
        let text = format!("{BODY}\0");
        let source = MemoryBuffer::create_from_memory_range_copy(text.as_bytes(), "test");
        let module = context.create_module_from_ir(source).unwrap();
        module.set_triple(&machinery.get_triple());
        module.set_data_layout(&machinery.get_target_data().get_data_layout());
        module
    }

    fn of(sanitizers: Sanitizers) -> Sanitize {
        Sanitize { sanitizers, ..Sanitize::default() }
    }

    fn fuzzing(level: u8) -> Sanitize {
        let flags = Coverage::INLINE_8BIT_COUNTERS | Coverage::PC_TABLE | Coverage::TRACE_CMP;
        Sanitize { coverage: Coverage { level, flags }, ..Sanitize::default() }
    }

    /// The module after marking its function with `off` turned off and instrumenting it.
    fn instrumented(sanitize: &Sanitize, off: Sanitizers) -> String {
        let context = Context::create();
        let machinery = machinery();
        let module = parsed(&context, &machinery);
        mark(&context, module.get_function(OWN).unwrap(), sanitize, off);
        module.verify().unwrap();
        instrument(&module, &machinery, sanitize).unwrap();
        module.verify().unwrap();
        module.print_to_string().to_string_lossy().into_owned()
    }

    /// The text of the function alone.
    fn own(module: &str) -> &str {
        let start = module.find("define i32 @blaze.own(").unwrap();
        let end = module[start..].find("\n}\n").unwrap();
        &module[start..start + end]
    }

    fn count(text: &str, call: &str) -> usize {
        text.matches(&format!("call void @{call}")).count()
    }

    #[test]
    fn each_sanitizer_marks_and_checks_and_none_without_it() {
        let mut sanitizers = vec![(Sanitizers::ADDRESS, "sanitize_address", "__asan_")];
        sanitizers.push((Sanitizers::THREAD, "sanitize_thread", "__tsan_"));
        if cfg!(target_os = "linux") {
            sanitizers.push((Sanitizers::MEMORY, "sanitize_memory", "__msan_"));
        }
        for &(sanitizer, attribute, calls) in &sanitizers {
            let text = instrumented(&of(sanitizer), Sanitizers::NONE);
            assert!(text.contains(attribute), "{text}");
            assert!(own(&text).contains(calls), "{text}");
            for &(other, attribute, calls) in &sanitizers {
                if other != sanitizer {
                    assert!(!text.contains(attribute) && !text.contains(calls), "{text}");
                }
            }
            assert!(!text.contains("__sanitizer_cov"), "{text}");
        }
        let plain = instrumented(&Sanitize::default(), Sanitizers::NONE);
        for &(_, attribute, calls) in &sanitizers {
            assert!(!plain.contains(attribute) && !plain.contains(calls), "{plain}");
        }
    }

    #[test]
    fn coverage_counts_and_traces_compares() {
        let text = instrumented(&fuzzing(3), Sanitizers::NONE);
        assert!(text.contains(SECTIONS[0]) && text.contains(PCS), "{text}");
        assert_eq!(count(own(&text), "__sanitizer_cov_trace_const_cmp4"), 1, "{text}");
        assert!(!text.contains("sanitize_address"), "{text}");
        let none = instrumented(&Sanitize::default(), Sanitizers::NONE);
        assert!(!none.contains("__sancov") && !none.contains("__sanitizer_cov"), "{none}");
    }

    /// SanitizerCoverage runs before AddressSanitizer: it counts and traces the function's own
    /// blocks and compares, none of the checks AddressSanitizer adds.
    #[test]
    fn coverage_runs_before_address_sanitizer() {
        let alone = instrumented(&fuzzing(3), Sanitizers::NONE);
        let both = Sanitize { sanitizers: Sanitizers::ADDRESS, ..fuzzing(3) };
        let both = instrumented(&both, Sanitizers::NONE);
        assert!(own(&both).contains("__asan_report"), "{both}");
        let compares = |text: &str| text.matches("__sanitizer_cov_trace_").count();
        assert_eq!(compares(own(&alone)), compares(own(&both)), "{alone}\n{both}");
        let counters = |text: &str| {
            let array = |it: &&str| it.starts_with("@__sancov_gen_") && it.contains(SECTIONS[0]);
            let line = text.lines().find(array).unwrap().to_owned();
            line[line.find('[').unwrap()..line.find(']').unwrap()].to_owned()
        };
        assert_eq!(counters(&alone), counters(&both));
    }

    #[test]
    fn a_function_with_its_sanitizer_off_is_not_marked() {
        let all = Sanitizers::ADDRESS.union(Sanitizers::THREAD);
        let text = instrumented(&of(all), Sanitizers::ADDRESS);
        assert!(!text.contains("sanitize_address") && !own(&text).contains("__asan_"), "{text}");
        assert!(text.contains("sanitize_thread") && own(&text).contains("__tsan_"), "{text}");
        let off = Sanitizers::ADDRESS.union(Sanitizers::COVERAGE);
        let text = instrumented(&Sanitize { sanitizers: Sanitizers::ADDRESS, ..fuzzing(3) }, off);
        assert!(text.contains("nosanitize_coverage"), "{text}");
        assert!(!own(&text).contains("__sanitizer_cov") && !own(&text).contains("__asan_"), "{text}");
    }

    #[test]
    fn nothing_asked_changes_nothing() {
        let context = Context::create();
        let machinery = machinery();
        let module = parsed(&context, &machinery);
        let before = module.write_bitcode_to_memory().as_slice().to_vec();
        mark(&context, module.get_function(OWN).unwrap(), &Sanitize::default(), Sanitizers::NONE);
        instrument(&module, &machinery, &Sanitize::default()).unwrap();
        assert_eq!(module.write_bitcode_to_memory().as_slice(), before);
        assert!(wanted(&Sanitize::default()).is_none());
        // A level LLVM has no coverage for asks for nothing.
        assert!(wanted(&fuzzing(0)).is_none());
    }

    /// The object of an instrumented function reads back with its counters and a PC table of
    /// its blocks, the entry first.
    #[test]
    fn the_pc_table_reads_back() {
        let target = crate::Target::selected().unwrap();
        if target.object_format == crate::ObjectFormat::Coff {
            return;
        }
        let context = Context::create();
        let machinery = machinery();
        let module = parsed(&context, &machinery);
        let sanitize = fuzzing(3);
        mark(&context, module.get_function(OWN).unwrap(), &sanitize, Sanitizers::NONE);
        instrument(&module, &machinery, &sanitize).unwrap();
        let object = machinery.write_to_memory_buffer(&module, FileType::Object).unwrap();
        let isa = crate::target().unwrap();
        let mut wants = crate::fragment::Wants::new(&**isa);
        let signature = cranelift_codegen::ir::Signature::new(isa.default_call_conv());
        let (code, covered) =
            super::super::emitted::read_covered(object.as_slice(), (OWN, signature.clone()), &mut wants)
                .unwrap();
        assert_eq!(covered.arrays[1..], [0, 0]);
        assert!(covered.blocks() >= 3, "{covered:?}");
        assert_eq!(covered.pcs.len(), covered.blocks() as usize, "{covered:?}");
        assert_eq!(covered.pcs[0], (0, 1));
        assert!(covered.pcs[1..].iter().all(|&(at, flags)| at > 0 && flags == 0), "{covered:?}");
        assert!(covered.pcs.iter().all(|&(at, _)| (at as usize) < code.bytes.len()));
        // Read without coverage, the same object is refused.
        let mut wants = crate::fragment::Wants::new(&**isa);
        let plain = super::super::emitted::read(object.as_slice(), &[(OWN, signature)], &mut wants).unwrap();
        assert!(plain[0].is_err());
    }
}
