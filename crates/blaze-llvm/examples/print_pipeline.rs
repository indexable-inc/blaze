//! Prints the pinned LLVM's own `default<O2>` pipeline text, as `opt -print-pipeline-passes`
//! prints it. It is printed for the host blaze targets first (arm64 macOS, `apple-m1`), then for
//! x86-64 Linux (`x86-64-v3`), because a target adds passes of its own. Each text follows a
//! `# <triple> <cpu>` line. `blaze-O2` (src/pipeline.rs) is derived from this text.
//!
//! Run once: `cargo run -p blaze-llvm --example print_pipeline > port27/llvm-O2-pipeline-23.1.4.txt`.

#![feature(allocator_api)]

use blaze_llvm::{default_pipeline, init, triple};
use blaze_own::Alloc;
use blaze_tier::{Arch, Machine, Os};

fn main() {
    init().unwrap_or_else(|e| panic!("{e}"));
    let alloc = Alloc::global();
    let machines = [
        (Machine { arch: Arch::Arm64, os: Os::MacOs, features: Machine::LSE }, "apple-m1"),
        (Machine { arch: Arch::X86_64, os: Os::Linux, features: 0 }, "x86-64-v3"),
    ];
    for (machine, cpu) in machines {
        let text = default_pipeline(&machine, cpu, 2, alloc).unwrap_or_else(|e| panic!("{e}"));
        println!("# {} {cpu}", triple(&machine));
        println!("{}", text.as_str());
    }
}
