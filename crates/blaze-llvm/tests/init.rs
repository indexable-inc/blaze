//! `init` is safe from any thread. Eight threads call it at once, and every one returns Ok only
//! once the targets are registered: a compile right after it is not `NotInitialized`.
//! Passes: "test init_from_many_threads_at_once ... ok".

#![feature(allocator_api)]

use blaze_llvm::{LlvmError, Pipeline, Remark, compile, init};
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::{Arch, Machine, Os};

const MAC: Machine = Machine { arch: Arch::Arm64, os: Os::MacOs, features: Machine::LSE };

#[test]
fn init_from_many_threads_at_once() {
    let start = std::sync::Barrier::new(8);
    std::thread::scope(|scope| {
        let threads: std::vec::Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    start.wait();
                    init().unwrap_or_else(|e| panic!("{e}"));
                    // Registered by the time `init` returns: the compile gets past the target
                    // check (it stops later, at the empty pipeline or not at all).
                    let alloc = Alloc::global();
                    let mut out: AVec<'_, u8> = Vec::new_in(alloc);
                    let mut rows: AVec<'_, Remark> = Vec::new_in(alloc);
                    let ir = b"define void @f() {\n  ret void\n}\n";
                    let result = compile(ir, &MAC, "apple-m1", Pipeline::BlazeO2, &mut out, &mut rows, alloc);
                    assert!(
                        !matches!(result, Err(LlvmError::NotInitialized | LlvmError::InitRacing)),
                        "init returned before registration: {result:?}"
                    );
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("no thread panicked");
        }
    });
}
