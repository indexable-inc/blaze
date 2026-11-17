//! `blaze-O2` against the pinned LLVM's live `default<O2>`: (a) it parses and runs, (b) every
//! pass in it appears in the default in the same relative order, so an LLVM bump that changes
//! the default is seen here, and (c) what it leaves out is exactly `REMOVED`.

#![feature(allocator_api)]

use blaze_llvm::pipeline::{self, REMOVED};
use blaze_llvm::{Pipeline, Remark, compile, default_pipeline, init};
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::{Arch, Machine, Os};

const MACHINES: [(Machine, &str); 2] = [
    (Machine { arch: Arch::Arm64, os: Os::MacOs, features: Machine::LSE }, "apple-m1"),
    (Machine { arch: Arch::X86_64, os: Os::Linux, features: 0 }, "x86-64-v3"),
];

/// (a) A module with a loop, an internal callee and an outside call compiles under `blaze-O2`
/// for both machines. Passes: "test the_pipeline_parses_and_runs ... ok".
#[test]
fn the_pipeline_parses_and_runs() {
    init().unwrap_or_else(|e| panic!("{e}"));
    let alloc = Alloc::global();
    let ir = "declare i64 @outside(i64)\n\
              define internal i64 @step(i64 %x) {\n  %y = mul i64 %x, 3\n  ret i64 %y\n}\n\
              define i64 @sum(ptr %p, i64 %n) {\nentry:\n  br label %loop\n\
              loop:\n  %i = phi i64 [ 0, %entry ], [ %i1, %loop ]\n  %acc = phi i64 [ 0, %entry ], [ %acc1, %loop ]\n\
              \x20 %q = getelementptr i64, ptr %p, i64 %i\n  %v = load i64, ptr %q, align 8\n\
              \x20 %w = call i64 @step(i64 %v)\n  %acc1 = add i64 %acc, %w\n  %i1 = add i64 %i, 1\n\
              \x20 %done = icmp eq i64 %i1, %n\n  br i1 %done, label %exit, label %loop\n\
              exit:\n  %r = call i64 @outside(i64 %acc1)\n  ret i64 %r\n}\n";
    for (machine, cpu) in MACHINES {
        let mut out: AVec<'_, u8> = Vec::new_in(alloc);
        let mut rows: AVec<'_, Remark> = Vec::new_in(alloc);
        compile(ir.as_bytes(), &machine, cpu, Pipeline::BlazeO2, &mut out, &mut rows, alloc)
            .unwrap_or_else(|e| panic!("{:?}: {e}", machine.arch));
        assert!(!out.is_empty(), "{:?}: an object", machine.arch);
    }
}

/// (b) and (c): `blaze-O2`'s tokens are the default's in order, minus exactly `REMOVED`.
/// Passes: "test the_pipeline_is_the_default_minus_the_removed_passes ... ok".
#[test]
fn the_pipeline_is_the_default_minus_the_removed_passes() {
    init().unwrap_or_else(|e| panic!("{e}"));
    let alloc = Alloc::global();
    let removed: std::vec::Vec<&str> = REMOVED.iter().map(|it| it.pass).collect();
    for (machine, cpu) in MACHINES {
        let default = default_pipeline(&machine, cpu, 2, alloc).unwrap_or_else(|e| panic!("{e}"));
        let ours: std::vec::Vec<&str> = pipeline::tokens(Pipeline::BlazeO2.passes(machine.arch)).collect();
        for token in &ours {
            assert!(!removed.contains(token), "{:?}: blaze-O2 holds the removed `{token}`", machine.arch);
        }
        // Walk the default: each token is either blaze-O2's next one or left out.
        let mut next = 0usize;
        let mut left_out: std::vec::Vec<&str> = std::vec::Vec::new();
        for token in pipeline::tokens(default.as_str()) {
            if ours.get(next) == Some(&token) {
                next += 1;
            } else {
                left_out.push(token);
            }
        }
        assert_eq!(
            next,
            ours.len(),
            "{:?}: blaze-O2's token {} (`{}`) is not in LLVM's default<O2> in this order: the default changed",
            machine.arch,
            next,
            ours.get(next).copied().unwrap_or("")
        );
        // Every occurrence is removed, so the left-out tokens, deduplicated in order, are `REMOVED`.
        let mut unique: std::vec::Vec<&str> = std::vec::Vec::new();
        for token in left_out {
            if !unique.contains(&token) {
                unique.push(token);
            }
        }
        assert_eq!(unique, removed, "{:?}: the passes left out are exactly REMOVED", machine.arch);
    }
}

/// The tokenizer keeps parameters whole and splits adaptors.
/// Passes: "test tokens_keep_parameters_whole ... ok".
#[test]
fn tokens_keep_parameters_whole() {
    let got: std::vec::Vec<&str> = pipeline::tokens("a,f<x;y>(b<c;d>,e),g").collect();
    assert_eq!(got, ["a", "f<x;y>", "(", "b<c;d>", "e", ")", "g"]);
}
