//! What a function's optimized code still does inside its loops, as LLVM says it: the report
//! the `blaze::vecreport` lint reads (docs/v2/plan/design/vecreport.md).
//!
//! Two kinds of finding, both read from the code LLVM is about to write, never guessed:
//!
//! - A branch out of a loop to a panic path: code that goes on to `unreachable`, which is what
//!   a bounds check, an overflow check, an `unwrap` or any other panic the optimizer could not
//!   prove dead is lowered to (a call of the panic function, `llvm.trap`, `unreachable`). The
//!   wrapper's `LLVMBlazeReportKept` finds them with LLVM's own `LoopInfo` once every pass has
//!   run, placed at the call.
//! - The loop vectorizer's remarks of a loop it left scalar, and why: caught by a diagnostic
//!   handler while the passes run (`LLVMBlazeReportStart`), as `-Rpass-missed=loop-vectorize`
//!   and `-Rpass-analysis=loop-vectorize` print them in clang.
//!
//! Each is placed by the line of its debug location, which LLVM's lowering makes the statement
//! of the code plus one ([`crate::debuginfo::LineRow`]); `v2::release` turns that into the
//! statement's position. Each carries its loop's heat: the loop header's count by LLVM's
//! block frequencies, hot or cold by the module's profile summary, where the code was built
//! with a profile.
//!
//! Asking for remarks only adds observers: a remark describes a decision and is made after
//! it, so the code is the same with or without them. `asking_for_the_report_changes_no_code`
//! holds LLVM to that on a vectorized loop.

#![forbid(unsafe_code)]

use inkwell::llvm_sys::transforms::pass_builder as ffi;

use super::Unhandled;

/// Whether a compile reports, and what its object keeps of the lines the report is read by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Asked {
    No,
    /// Report, and keep the lines in the object: it is built with debug information on a
    /// format whose line table is read back.
    Lined,
    /// Report, and strip the lines before the object is written: they were made for the
    /// report alone, so the object is the one a build without it writes.
    Unlined,
}

impl Asked {
    /// For a function built with `report`, with debug information if `debug`, for `target`.
    pub(crate) fn of(report: bool, debug: bool, target: &crate::Target) -> Asked {
        let read_back =
            matches!(target.object_format, crate::ObjectFormat::Elf | crate::ObjectFormat::Coff);
        match (report, debug && read_back) {
            (false, _) => Asked::No,
            (true, true) => Asked::Lined,
            (true, false) => Asked::Unlined,
        }
    }
}

/// What one finding is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Found {
    /// A branch out of a loop to a panic path, which calls `callee`.
    Kept { callee: Called },
    /// The loop vectorizer left the loop scalar: its "missed" remark.
    Missed { name: String, message: String },
    /// Why: its "analysis" remark, of the loop its next missed remark names.
    Analysis { name: String, message: String },
}

/// What a panic path calls first.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Called {
    /// The function request of this number (`blaze.f.N`): the panic function a check calls.
    Request(u32),
    /// A function of the module by another name.
    Named(String),
    /// Nothing before `llvm.trap`.
    Trap,
    /// A function through a pointer.
    Pointer,
}

impl Called {
    fn of(name: String) -> Called {
        if name.is_empty() {
            return Called::Pointer;
        }
        if name == "llvm.trap" {
            return Called::Trap;
        }
        match name.strip_prefix("blaze.f.").and_then(|it| it.parse().ok()) {
            Some(request) => Called::Request(request),
            None => Called::Named(name),
        }
    }
}

/// How hot a finding's loop is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Heat {
    /// Built without a profile.
    Unprofiled,
    /// Built with one, and no count reaches the loop: it is gone (unrolled whole, vectorized
    /// into another), or of code the profile never counted.
    Uncounted,
    /// The loop header's count, not hot by the profile summary.
    Cold(u64),
    /// The loop header's count, hot by the profile summary (`ProfileSummaryInfo::isHotCount`).
    Hot(u64),
}

/// A finding as LLVM placed it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Raw {
    pub(crate) found: Found,
    /// The line of its code (the statement plus one); 0 for code of no statement.
    pub(crate) line: u32,
    /// The line of its loop's start; 0 where not known.
    pub(crate) loop_line: u32,
    pub(crate) heat: Heat,
}

impl Raw {
    /// A row of the wrapper's (`LLVMBlazeReportRow`), its strings copied.
    pub(super) fn of(
        kind: u8,
        heat: u8,
        line: u32,
        loop_line: u32,
        count: u64,
        name: String,
        message: String,
    ) -> Result<Raw, Unhandled> {
        let found = match kind {
            ffi::LLVMBlazeFoundKept => Found::Kept { callee: Called::of(name) },
            ffi::LLVMBlazeFoundMissed => Found::Missed { name, message },
            ffi::LLVMBlazeFoundAnalysis => Found::Analysis { name, message },
            // alloc: the error's message
            other => {
                return Err(Unhandled::Llvm(format!(
                    "a report row of kind {other}, which blaze's wrapper never makes"
                )));
            }
        };
        let heat = match heat {
            ffi::LLVMBlazeHeatUnprofiled => Heat::Unprofiled,
            ffi::LLVMBlazeHeatUncounted => Heat::Uncounted,
            ffi::LLVMBlazeHeatCold => Heat::Cold(count),
            ffi::LLVMBlazeHeatHot => Heat::Hot(count),
            // alloc: the error's message
            other => {
                return Err(Unhandled::Llvm(format!(
                    "a report row of heat {other}, which blaze's wrapper never makes"
                )));
            }
        };
        Ok(Raw { found, line, loop_line, heat })
    }
}

#[cfg(test)]
mod tests {
    use inkwell::{
        context::Context, memory_buffer::MemoryBuffer, module::Module, targets::FileType,
    };

    use super::*;
    use crate::{
        backend::{Optimization, Passes, Selection},
        llvm::{Machine, Machinery, OWN},
    };

    fn parsed<'ctx>(context: &'ctx Context, text: &str) -> Module<'ctx> {
        let text = format!("{text}\0");
        let source = MemoryBuffer::create_from_memory_range_copy(text.as_bytes(), "test");
        context.create_module_from_ir(source).unwrap()
    }

    fn machine() -> Machinery {
        let optimization =
            Optimization { passes: Passes::Aggressive, selection: Selection::Optimized };
        Machinery::new(Machine::create(crate::Target::x86_64_linux(), optimization).unwrap())
    }

    /// The report of `text`'s own function after `passes`.
    fn report(text: &str, passes: &str) -> Vec<Raw> {
        let context = Context::create();
        let module = parsed(&context, text);
        let machine = machine();
        let ((), found) = machine
            .reported(|| {
                machine.run(&module, passes, true)?;
                Ok(((), module.get_function(OWN).expect("its function")))
            })
            .unwrap();
        found
    }

    /// The debug metadata of the test modules: one unit, one subprogram `!10` of the
    /// function, lines `!3` (3), `!5` (5) and `!7` (7) in it.
    const LINES: &str = r#"
!llvm.dbg.cu = !{!0}
!llvm.module.flags = !{!1}
!0 = distinct !DICompileUnit(language: DW_LANG_Rust, file: !2, producer: "blaze", isOptimized: true, runtimeVersion: 0, emissionKind: LineTablesOnly)
!1 = !{i32 2, !"Debug Info Version", i32 3}
!2 = !DIFile(filename: "blaze.own", directory: "")
!10 = distinct !DISubprogram(name: "blaze.own", scope: !2, file: !2, line: 0, type: !11, spFlags: DISPFlagDefinition | DISPFlagOptimized, unit: !0)
!11 = !DISubroutineType(types: !{})
!3 = !DILocation(line: 3, scope: !10)
!5 = !DILocation(line: 5, scope: !10)
!7 = !DILocation(line: 7, scope: !10)
"#;

    /// `for i in 0..n { sum += v[i] }` with `v` of length `len`, as blaze lowers it: the
    /// check of each index branches out of the loop to the panic function (line 7).
    const CHECKED: &str = r#"
declare void @"blaze.f.1"(i64, i64)
declare void @llvm.trap()

define i64 @"blaze.own"(ptr %v, i64 %n, i64 %len) !dbg !10 {
entry:
  br label %head, !dbg !3
head:
  %i = phi i64 [ 0, %entry ], [ %next, %body ]
  %sum = phi i64 [ 0, %entry ], [ %added, %body ]
  %more = icmp ult i64 %i, %n, !dbg !3
  br i1 %more, label %check, label %done, !dbg !3
check:
  %inside = icmp ult i64 %i, %len, !dbg !5
  br i1 %inside, label %body, label %panic, !dbg !5
body:
  %at = getelementptr inbounds i64, ptr %v, i64 %i, !dbg !5
  %x = load i64, ptr %at, !dbg !5
  %added = add i64 %sum, %x, !dbg !5
  %next = add nuw i64 %i, 1, !dbg !3
  br label %head, !dbg !3
panic:
  call void @"blaze.f.1"(i64 %i, i64 %len), !dbg !7
  call void @llvm.trap(), !dbg !7
  unreachable, !dbg !7
done:
  ret i64 %sum, !dbg !3
}
"#;

    #[test]
    fn a_check_kept_in_a_loop_is_found_at_its_call() {
        let found = report(&format!("{CHECKED}{LINES}"), "function(simplifycfg)");
        let kept: Vec<_> =
            found.iter().filter(|it| matches!(it.found, Found::Kept { .. })).collect();
        assert_eq!(kept.len(), 1, "{found:?}");
        assert_eq!(kept[0].found, Found::Kept { callee: Called::Request(1) });
        assert_eq!((kept[0].line, kept[0].loop_line), (7, 3));
        assert_eq!(kept[0].heat, Heat::Unprofiled);
    }

    /// The same loop after `assert!(n <= len)`: the check of `n` against `len` is before the
    /// loop, and LLVM drops the one inside, so nothing branches out of the loop to a panic.
    #[test]
    fn a_check_proven_before_the_loop_is_not_found() {
        let proven = CHECKED.replace(
            "entry:\n  br label %head, !dbg !3",
            // Its own panic block: the loop's reads `%i`, which the entry does not dominate.
            "entry:\n  %fits = icmp ule i64 %n, %len, !dbg !3\n  br i1 %fits, label %head, label %short, !dbg !3\n\
             short:\n  call void @\"blaze.f.1\"(i64 %n, i64 %len), !dbg !7\n  call void @llvm.trap(), !dbg !7\n  \
             unreachable, !dbg !7",
        );
        let found = report(
            &format!("{proven}{LINES}"),
            "function(instcombine,simplifycfg,indvars,constraint-elimination,instcombine,simplifycfg)",
        );
        assert!(!found.iter().any(|it| matches!(it.found, Found::Kept { .. })), "{found:?}");
    }

    /// A loop that calls a function LLVM knows nothing of: the vectorizer leaves it scalar and
    /// says why, both at the loop's line.
    #[test]
    fn a_loop_left_scalar_says_why() {
        let text = r#"
declare void @"blaze.f.2"(i64)

define void @"blaze.own"(i64 %n) !dbg !10 {
entry:
  br label %head, !dbg !3
head:
  %i = phi i64 [ 0, %entry ], [ %next, %head ]
  call void @"blaze.f.2"(i64 %i), !dbg !5
  %next = add nuw i64 %i, 1, !dbg !3
  %more = icmp ult i64 %next, %n, !dbg !3
  br i1 %more, label %head, label %done, !dbg !3
done:
  ret void, !dbg !3
}
"#;
        let found = report(&format!("{text}{LINES}"), "function(loop-vectorize)");
        let missed = found.iter().find(|it| matches!(it.found, Found::Missed { .. }));
        let missed = missed.unwrap_or_else(|| panic!("no missed remark: {found:?}"));
        assert_ne!(missed.loop_line, 0, "{found:?}");
        let why = found.iter().find(|it| matches!(it.found, Found::Analysis { .. }));
        let why = why.unwrap_or_else(|| panic!("no reason: {found:?}"));
        assert_eq!(why.loop_line, missed.loop_line, "the reason is of the loop: {found:?}");
        let Found::Analysis { message, .. } = &why.found else { unreachable!() };
        assert!(message.contains("call"), "{message}");
    }

    /// The benchmark: `default<O3>` over the checked loop, with and without the report, as
    /// many times each, interleaved. Prints both and the ratio; run with `--ignored
    /// --nocapture`. A measurement, not a check: it fails only if the report is over twice
    /// the passes' time.
    #[test]
    #[ignore = "a benchmark"]
    fn the_report_costs_little_beside_the_passes() {
        const ROUNDS: u32 = 200;
        let text = format!("{CHECKED}{LINES}");
        let machine = machine();
        let once = |reporting: bool| {
            let context = Context::create();
            let module = parsed(&context, &text);
            let started = std::time::Instant::now();
            if reporting {
                machine
                    .reported(|| {
                        Ok((
                            machine.run(&module, "default<O3>", true)?,
                            module.get_function(OWN).unwrap(),
                        ))
                    })
                    .unwrap();
            } else {
                machine.run(&module, "default<O3>", true).unwrap();
            }
            started.elapsed()
        };
        let (mut plain, mut reported) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
        for _ in 0..ROUNDS {
            plain += once(false);
            reported += once(true);
        }
        let ratio = reported.as_secs_f64() / plain.as_secs_f64();
        println!(
            "vecreport: {ROUNDS} runs, plain {plain:?}, reported {reported:?}, ratio {ratio:.3}"
        );
        assert!(ratio < 2.0, "the report took {ratio:.2} times the passes alone");
    }

    /// The same object with and without the report, of a loop LLVM vectorizes.
    #[test]
    fn asking_for_the_report_changes_no_code() {
        let text = format!(
            "{}{LINES}",
            r#"
define void @"blaze.own"(ptr noalias %a, ptr noalias %b, i64 %n) !dbg !10 {
entry:
  %any = icmp ne i64 %n, 0, !dbg !3
  br i1 %any, label %head, label %done, !dbg !3
head:
  %i = phi i64 [ 0, %entry ], [ %next, %head ]
  %from = getelementptr inbounds i32, ptr %b, i64 %i, !dbg !5
  %x = load i32, ptr %from, !dbg !5
  %y = mul i32 %x, 3, !dbg !5
  %to = getelementptr inbounds i32, ptr %a, i64 %i, !dbg !5
  store i32 %y, ptr %to, !dbg !5
  %next = add nuw i64 %i, 1, !dbg !3
  %more = icmp ult i64 %next, %n, !dbg !3
  br i1 %more, label %head, label %done, !dbg !3
done:
  ret void, !dbg !3
}
"#
        );
        let object = |reporting: bool| {
            let context = Context::create();
            let module = parsed(&context, &text);
            let machine = machine();
            let run = || machine.run(&module, "default<O3>", true);
            if reporting {
                let ((), found) = machine
                    .reported(|| Ok((run()?, module.get_function(OWN).expect("its function"))))
                    .unwrap();
                assert!(
                    !found.iter().any(|it| matches!(it.found, Found::Missed { .. })),
                    "{found:?}"
                );
            } else {
                run().unwrap();
            }
            machine.write_to_memory_buffer(&module, FileType::Object).unwrap().as_slice().to_vec()
        };
        assert_eq!(object(true), object(false));
    }
}
