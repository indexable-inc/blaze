//! s7-proc-02: `std::env::args` of a program `blaze run` started with no arguments.
//!
//! std/src/env.rs, `args`: "The first element is traditionally the path of the executable".
//! blaze's run spec (docs/v2/plan/design/run.md, "One spec, written down") gives the program its
//! own arguments and nothing of blaze's: the program sees exactly one argument, its name, and it is
//! not empty (planner's choice, port27/tempo-run/LADDER.md piece L3: argv[0] is the binary
//! target's name). On macOS std reads them from `_NSGetArgc`/`_NSGetArgv`
//! (std/src/sys/args/unix.rs), so a program run inside blaze's own process would see blaze's
//! arguments (`blaze run`) and fail this rung: it passes only when the program is its own process.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    println!("arguments after the program's name: {}", args.len().saturating_sub(1));
    println!("a program name is given: {}", args.first().is_some_and(|a| !a.is_empty()));
    println!("{:?}", std::env::args().skip(1).collect::<Vec<_>>());
    println!("{}", std::env::args_os().count() == args.len());
}
