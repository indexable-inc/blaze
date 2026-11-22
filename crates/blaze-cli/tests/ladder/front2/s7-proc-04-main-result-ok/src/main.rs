//! s7-proc-04: `fn main() -> Result<(), Box<dyn Error>>` returning `Ok(())`: the `Termination`
//! impl for `Result<T: Termination, E: Debug>` reports `Ok(val)` as `val.report()`
//! (`ExitCode::SUCCESS` for `()`), so the status is 0. (Its `Err` side, "Error: {err:?}" on
//! standard error and status 1, is the next rung in LADDER.md: today's command runner reads a
//! status 1 with words on standard error as blaze refusing.) tempo's own `main` has this shape:
//! `fn main() -> eyre::Result<()>` (bin/tempo/src/main.rs).

use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let n: i32 = "12".parse()?;
    println!("{}", n * 2);
    let v: Vec<u8> = Vec::with_capacity(1);
    println!("{}", v.capacity() >= 1);
    Ok(())
}
