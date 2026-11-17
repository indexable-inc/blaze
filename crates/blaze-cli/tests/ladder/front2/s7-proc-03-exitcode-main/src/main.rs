//! s7-proc-03: `fn main() -> ExitCode`. The `Termination` impl for `ExitCode` reports it
//! unchanged (std/src/process.rs: `impl Termination for ExitCode { fn report(self) -> ExitCode
//! { self } }`), and `lang_start` returns `main().report().to_i32()` (std/src/rt.rs), so the
//! process ends with status 42. First to need: `lang_start::<ExitCode>` (the entry instantiated
//! at `main`'s return type), not only at `()`.

use std::process::ExitCode;

fn main() -> ExitCode {
    println!("returning 42");
    ExitCode::from(42)
}
