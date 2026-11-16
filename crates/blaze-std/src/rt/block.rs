//! The process's start block: the command line and environment where the kernel left them
//! (docs/v2/plan/design/std.md, "What a program pays for at start"). Nothing is copied: the
//! arguments are checked to be UTF-8 and kept as `&'static str`s over the kernel's bytes, and the
//! environment's pointer array is taken as `&'static CStr`s; a variable is found by a scan when
//! asked ([`crate::env::Env`]).
//!
//! Where each comes from:
//! - macOS: `_NSGetArgc`, `_NSGetArgv` and `_NSGetEnviron`, libSystem's `crt_externs.h` (rule 45:
//!   Darwin-only calls, public but undocumented outside that header; Rust std reads its own
//!   arguments the same way). They are the values `start` handed `main`, on the stack the kernel
//!   built at exec.
//! - Linux (glibc): glibc calls each `.init_array` function with `(argc, argv, envp)`, as it
//!   calls `main`; [`capture`] keeps them (Rust std does the same for its own `args`). Another
//!   C library is refused by name at start: no other source is guessed at.
//!
//! Why `'static` is sound: the argument and environment strings the kernel placed at exec live
//! on the initial stack's top, above every frame, until the process exits; nothing frees them.
//! The environment's pointer array is copied here (pointers only), so a later change of the
//! array is not seen. The one assumption: no code in the process replaced a variable before this
//! snapshot with a string it later frees (`setenv`/`putenv` from a library's constructor, or
//! Rust's `set_var`, which is `unsafe` for this reason and which bstd never calls).

use alloc::{boxed::Box, format, string::String, vec::Vec};
use core::ffi::{CStr, c_char, c_int};

/// The command line and environment the process was started with, for its whole life.
pub(crate) struct Block {
    pub(crate) args: Box<[&'static str]>,
    pub(crate) env: Box<[&'static CStr]>,
}

#[cfg(target_os = "macos")]
mod darwin {
    use core::ffi::{c_char, c_int};

    // libSystem's crt_externs.h (rule 45: Darwin-only, tested to exist below).
    unsafe extern "C" {
        pub(super) fn _NSGetArgc() -> *mut c_int;
        pub(super) fn _NSGetArgv() -> *mut *mut *mut c_char;
        pub(super) fn _NSGetEnviron() -> *mut *mut *mut c_char;
    }
}

/// `(argc, argv, envp)` as glibc handed them to [`capture`], before `main`.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod glibc {
    use core::ffi::{c_char, c_int};
    use core::sync::atomic::{AtomicIsize, AtomicPtr, Ordering};

    static ARGC: AtomicIsize = AtomicIsize::new(-1);
    static ARGV: AtomicPtr<*const c_char> = AtomicPtr::new(core::ptr::null_mut());
    static ENVP: AtomicPtr<*const c_char> = AtomicPtr::new(core::ptr::null_mut());

    /// What glibc calls each `.init_array` entry with (its `_dl_init` and `__libc_csu_init`
    /// pass `main`'s three arguments); the number orders it among Rust std's own (`.00099`).
    #[used]
    #[unsafe(link_section = ".init_array.00098")]
    static CAPTURE: extern "C" fn(c_int, *const *const c_char, *const *const c_char) = capture;

    extern "C" fn capture(argc: c_int, argv: *const *const c_char, envp: *const *const c_char) {
        ARGV.store(argv.cast_mut(), Ordering::Relaxed);
        ENVP.store(envp.cast_mut(), Ordering::Relaxed);
        ARGC.store(argc as isize, Ordering::Release);
    }

    /// `None` when glibc never called [`capture`].
    pub(super) fn get() -> Option<(c_int, *const *const c_char, *const *const c_char)> {
        let argc = ARGC.load(Ordering::Acquire);
        (argc >= 0).then(|| {
            (argc as c_int, ARGV.load(Ordering::Relaxed).cast_const(), ENVP.load(Ordering::Relaxed).cast_const())
        })
    }
}

/// `argc`, `argv` and `envp` as the platform keeps them.
#[cfg(target_os = "macos")]
fn raw() -> Result<(c_int, *const *const c_char, *const *const c_char), String> {
    // SAFETY: the three calls take nothing and return pointers to libSystem's own variables,
    // set by `start` before any initialiser ran; each is valid for the process's life.
    unsafe {
        Ok((
            *darwin::_NSGetArgc(),
            (*darwin::_NSGetArgv()).cast_const().cast(),
            (*darwin::_NSGetEnviron()).cast_const().cast(),
        ))
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn raw() -> Result<(c_int, *const *const c_char, *const *const c_char), String> {
    glibc::get().ok_or_else(|| {
        // alloc: an error's message.
        String::from("bstd::rt: glibc never called the start block's `.init_array` entry, so the command line is not known")
    })
}

#[cfg(not(any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))))]
fn raw() -> Result<(c_int, *const *const c_char, *const *const c_char), String> {
    // alloc: an error's message.
    Err(String::from("bstd::rt: this target's start block is not read yet (macOS and Linux with glibc are)"))
}

/// The start block, read once: an argument that is not UTF-8 is refused, naming its place
/// (`Args` holds text).
pub(crate) fn read() -> Result<&'static Block, String> {
    let (argc, argv, envp) = raw()?;
    let count = usize::try_from(argc).unwrap_or(0);
    // alloc: once per process, one pointer and length per argument.
    let mut args = Vec::with_capacity(count);
    for at in 0..count {
        // SAFETY: `argv` holds `argc` pointers, each to a NUL-terminated string on the initial
        // stack, alive until exit (the module's doc).
        let arg: &'static CStr = unsafe { CStr::from_ptr(*argv.add(at)) };
        match arg.to_str() {
            Ok(text) => args.push(text),
            Err(_) => {
                // alloc: an error's message.
                return Err(format!(
                    "bstd::entry: argument {at} of the command line is not UTF-8 ({}): `Args` holds text",
                    arg.to_string_lossy()
                ));
            }
        }
    }
    let mut env = Vec::new();
    if !envp.is_null() {
        let mut at = 0;
        loop {
            // SAFETY: `envp` is a NULL-terminated array of pointers to NUL-terminated strings
            // (the module's doc says why each lives until exit).
            let entry = unsafe { *envp.add(at) };
            if entry.is_null() {
                break;
            }
            // SAFETY: as above.
            env.push(unsafe { CStr::from_ptr(entry) });
            at += 1;
        }
    }
    // alloc: once per process, the block, kept until exit.
    Ok(Box::leak(Box::new(Block { args: args.into_boxed_slice(), env: env.into_boxed_slice() })))
}

#[cfg(test)]
mod tests {
    /// The block a test process was started with: its first argument is its own path, and
    /// its environment holds what Rust std sees.
    #[test]
    fn the_block_is_what_the_process_was_started_with() {
        let block = super::read().expect("the start block");
        let std_args: alloc::vec::Vec<_> = super::super::std::env::args().collect();
        assert_eq!(block.args.len(), std_args.len());
        assert!(block.args.iter().zip(&std_args).all(|(a, b)| *a == b.as_str()));
        let path = super::super::std::env::var("PATH").expect("a test runs with PATH set");
        let found = block.env.iter().find_map(|it| it.to_bytes().strip_prefix(b"PATH=")).expect("PATH in the block");
        assert_eq!(found, path.as_bytes());
        let env = crate::env::Env::new(crate::env::Vars::process(block), crate::Plant::None);
        let name = crate::env::VarName::new("PATH").expect("a name");
        assert_eq!(env.get(&name), Ok(Some(path.as_str())), "`Env` finds it by a scan of the block");
        let args = crate::args::Args::new(crate::args::ArgList::process(block));
        assert_eq!(args.get(0), std_args.first().map(alloc::string::String::as_str));
    }
}
