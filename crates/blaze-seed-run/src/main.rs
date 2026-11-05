//! `blaze-seed-run [--memory-max=SIZE] SEED.wasm [ARG...]`: runs the seed with these arguments
//! (the seed's path first) and this process's environment, and exits with its code. SIZE is
//! the address space its memory reserves and the most it may grow to, in bytes or with a `K`,
//! `M`, `G` or `T` suffix (powers of 1024), 8 TiB by default. How much of its memory it held
//! goes to stderr, for the plan's memory measurements.

use std::process::ExitCode;

fn main() -> ExitCode {
    const USAGE: &str = "usage: blaze-seed-run [--memory-max=SIZE] SEED.wasm [ARG...]";
    let mut args = std::env::args_os().skip(1).peekable();
    let mut memory_max = blaze_seed_run::MEMORY_MAX;
    if let Some(size) = args
        .peek()
        .and_then(|first| first.to_str())
        .and_then(|first| first.strip_prefix("--memory-max="))
    {
        match size_of(size) {
            Some(bytes) => memory_max = bytes,
            None => {
                eprintln!(
                    "blaze-seed-run: --memory-max={size} is not a size (bytes, or a number with K, M, G or T)\n{USAGE}"
                );
                return ExitCode::from(2);
            }
        }
        args.next();
    }
    let Some(seed) = args.next() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let bytes = match std::fs::read(&seed) {
        Ok(bytes) => bytes,
        Err(why) => {
            eprintln!("blaze-seed-run: reading the seed {}: {why}", seed.display());
            return ExitCode::from(2);
        }
    };
    let config = blaze_seed_run::Config {
        args: std::iter::once(seed).chain(args).collect(), // alloc: once per run
        env: std::env::vars_os().collect(),                // alloc: once per run
        memory_max,
    };
    match blaze_seed_run::run(&bytes, &config) {
        Ok(exit) => {
            eprintln!(
                "blaze-seed-run: exit {}, memory {} bytes, {} held",
                exit.code, exit.memory_bytes, exit.held_bytes
            );
            ExitCode::from(u8::try_from(exit.code).unwrap_or(u8::MAX))
        }
        Err(why) => {
            eprintln!("blaze-seed-run: {why}");
            ExitCode::FAILURE
        }
    }
}

/// `size` in bytes: a number, with an optional `K`, `M`, `G` or `T` (powers of 1024).
fn size_of(size: &str) -> Option<u64> {
    let (digits, shift) = match size.as_bytes().last()? {
        b'K' | b'k' => (&size[..size.len() - 1], 10),
        b'M' | b'm' => (&size[..size.len() - 1], 20),
        b'G' | b'g' => (&size[..size.len() - 1], 30),
        b'T' | b't' => (&size[..size.len() - 1], 40),
        _ => (size, 0),
    };
    digits.parse::<u64>().ok()?.checked_mul(1 << shift).filter(|&bytes| bytes > 0)
}
