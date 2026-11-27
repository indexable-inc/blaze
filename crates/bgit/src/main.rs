//! Thin CLI entry point over `libbgit` (`bgit::run_bgit_to_writers`).

#![forbid(unsafe_code)]

use std::{
    alloc::Global,
    env,
    io::{self, IsTerminal as _},
    path::PathBuf,
    process::ExitCode,
};

use blaze_own::Erased;

fn main() -> ExitCode {
    let erased = Erased::new(Global);
    let alloc = erased.handle();
    let owned_args: Vec<String> = env::args().skip(1).collect();
    let arg_refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let use_color = io::stdout().is_terminal();

    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    let code = bgit::run_bgit_to_writers(&arg_refs, &cwd, use_color, &mut stdout, &mut stderr, alloc);
    ExitCode::from(code.clamp(0, 255) as u8)
}
