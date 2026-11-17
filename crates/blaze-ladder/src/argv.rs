//! The program's argument vector: the one source of what a started program is told about its
//! command line (docs/v2/plan/design/run.md, "What a program is given": the run spec holds "its
//! arguments"; bstd's `Args`: "the program's own name first").
//!
//! Every start reads this one value (rule 30):
//! - bstd's hosted entry (`bstd_hosted_start(table)`): the host's `arg` call answers
//!   [`Argv::get`], so the program's `args: Args` grant sees [`Argv::strings`];
//! - a plain C entry, `extern "C" fn main(argc: i32, argv: *const *const c_char) -> i32`: called
//!   with [`Argv::c`], the same strings as C wants them (each ended by a NUL, the array ended by a
//!   null pointer);
//! - a Rust program's start, `lang_start::<T>(main, argc, argv, 0)` through the C entry shim
//!   (front2/start.rs, LADDER.md L1b): the shim takes the same two words as a C `main`.
//!
//! `argv[0]` is the program's name: the package's, as the run parent names the child
//! (blaze-cli own_process.rs `arg0: package`), then the words after `blaze run --`.
//!
//! The C form is made once, in the allocator the caller hands in (D102), and lives as long as
//! the [`Argv`]: a [`CArgs`] taken from it points into it.

use core::fmt;
use core::ptr::NonNull;

use blaze_own::{Alloc, AllocFailure, Vec};

/// The program's command line: its name, then its arguments, and the same as C's `argc` and
/// `argv`.
///
/// # Invariants
///
/// 1. `text` holds `name` and each of `rest`, in order, each followed by one NUL byte, and none
///    holds a NUL of its own ([`Argv::new_in`] refuses one).
/// 2. `pointers` holds `1 + rest.len()` pointers, the `i`th to the first byte of argument `i`
///    in `text`, then one null pointer: C's `argv` with `argv[argc] == NULL` (C17 5.1.2.2.1:
///    "argv[argc] shall be a null pointer").
/// 3. Neither vector grows after `new_in` returns, so the pointers stay where they point until
///    the `Argv` is dropped (moving the `Argv` moves no heap bytes).
/// 4. `1 + rest.len() <= i32::MAX`: `argc` is a C `int`.
pub struct Argv<'a> {
    name: &'a str,
    rest: &'a [&'a str],
    /// `1 + rest.len()`, checked to fit (invariant 4).
    argc: i32,
    /// Held, never read again: the bytes `pointers` point into (invariants 1 and 3).
    _text: Vec<u8, Alloc<'a>>,
    pointers: Vec<*const u8, Alloc<'a>>,
}

/// C's `argc` and `argv` of an [`Argv`]: two machine words, as an entry is called with them.
///
/// The pointers are valid only while the `Argv` they came from lives: the caller of
/// [`Argv::c`] keeps it alive across every call that is given these (place.rs `Entry::Main`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CArgs {
    /// The count of arguments, the program's name included: never negative.
    pub argc: i32,
    /// `argc + 1` pointers, each to a NUL-ended string, the last null.
    pub argv: NonNull<*const u8>,
}

/// Why a command line cannot be given to a program.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArgvError {
    /// Argument `index` (0 is the program's name) holds a NUL byte: a C string ends at its
    /// first NUL, so the program would be told a shorter argument than was given.
    Nul { index: usize },
    /// More arguments than a C `int` counts.
    TooMany { count: usize },
    /// The vector's memory could not be had from the allocator handed in.
    Alloc(AllocFailure),
}

impl fmt::Display for ArgvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArgvError::Nul { index: 0 } => write!(f, "the program's name holds a NUL byte, which no C string can carry"),
            ArgvError::Nul { index } => write!(f, "the program's argument {index} holds a NUL byte, which no C string can carry"),
            ArgvError::TooMany { count } => write!(f, "{count} arguments are more than a C `int` (`argc`) counts"),
            ArgvError::Alloc(failure) => write!(f, "the program's argument vector: {failure}"),
        }
    }
}

impl<'a> Argv<'a> {
    /// The command line `name`, then `rest`, in both forms, its C form in `alloc`.
    pub fn new_in(name: &'a str, rest: &'a [&'a str], alloc: Alloc<'a>) -> Result<Argv<'a>, ArgvError> {
        let count = rest.len().checked_add(1).ok_or(ArgvError::TooMany { count: usize::MAX })?;
        let argc = i32::try_from(count).map_err(|_| ArgvError::TooMany { count })?;
        // Each argument's bytes and its NUL; refused before anything is allocated.
        let mut bytes = 0usize;
        let mut index = 0usize;
        for arg in core::iter::once(name).chain(rest.iter().copied()) {
            if arg.as_bytes().contains(&0) {
                return Err(ArgvError::Nul { index });
            }
            bytes = bytes.checked_add(arg.len()).and_then(|b| b.checked_add(1)).ok_or(ArgvError::Alloc(AllocFailure::CapacityOverflow {
                what: "u8",
                len: bytes,
                additional: arg.len(),
            }))?;
            index += 1;
        }
        // Exactly sized, so neither vector reallocates after this (invariant 3).
        let mut text: Vec<u8, Alloc<'a>> = Vec::try_with_capacity_in(bytes, alloc).map_err(ArgvError::Alloc)?;
        let mut starts: Vec<usize, Alloc<'a>> = Vec::try_with_capacity_in(count, alloc).map_err(ArgvError::Alloc)?;
        for arg in core::iter::once(name).chain(rest.iter().copied()) {
            starts.push(text.len());
            text.extend_from_slice(arg.as_bytes());
            text.push(0u8);
        }
        let mut pointers: Vec<*const u8, Alloc<'a>> = Vec::try_with_capacity_in(count + 1, alloc).map_err(ArgvError::Alloc)?;
        // From the mutable base: C lets a program write its argument strings (C17 5.1.2.2.1:
        // "the strings pointed to by the argv array shall be modifiable by the program"), and
        // nothing in Rust reads `text` again while a program runs ([`Argv::strings`] reads the
        // borrowed originals).
        let base = text.as_mut_ptr();
        for &start in starts.iter() {
            // In bounds: `start < text.len()` (each start is followed by at least its NUL).
            pointers.push(base.wrapping_add(start).cast_const());
        }
        pointers.push(core::ptr::null());
        Ok(Argv { name, rest, argc, _text: text, pointers })
    }

    /// The count of arguments, the program's name included.
    pub fn len(&self) -> usize {
        self.rest.len() + 1
    }

    /// Never: the program's name is always there.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Argument `at`, 0 being the program's name.
    pub fn get(&self, at: usize) -> Option<&'a str> {
        match at {
            0 => Some(self.name),
            _ => self.rest.get(at - 1).copied(),
        }
    }

    /// Every argument in order, the program's name first: what a hosted program's `Args` reads.
    pub fn strings(&self) -> impl Iterator<Item = &'a str> + '_ {
        core::iter::once(self.name).chain(self.rest.iter().copied())
    }

    /// C's `argc` and `argv`: valid while `self` lives (invariant 3); the program may write
    /// through them, as C allows.
    pub fn c(&mut self) -> CArgs {
        // Not empty (invariant 2: at least the name's pointer and the null), so the slice's
        // address is the first pointer's; from `&mut`, as the program may write the array too.
        let argv = NonNull::from(self.pointers.as_mut_slice()).cast::<*const u8>();
        CArgs { argc: self.argc, argv }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The C form holds the strings the hosted form gives, each ended by a NUL, the array ended
    /// by a null: C17 5.1.2.2.1 ("argv[argc] shall be a null pointer").
    #[test]
    fn the_c_form_holds_the_same_strings_and_ends_in_null() {
        // The test's one allocator, made at its top: a frame, its memory from the OS.
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let rest = ["one", "", "three"];
        let mut argv = Argv::new_in("ladder", &rest, erased.handle()).unwrap_or_else(|e| panic!("ARGV: {e}"));
        let c = argv.c();
        assert_eq!(c.argc, 4);
        let mut seen: Vec<&str, Alloc<'_>> = Vec::new_in(erased.handle());
        for i in 0..=4usize {
            // SAFETY: `argv` holds `argc + 1` pointers (invariant 2), alive while `argv` is.
            let p = unsafe { *c.argv.as_ptr().add(i) };
            if i == 4 {
                assert!(p.is_null(), "argv[argc] is not null");
                break;
            }
            // SAFETY: each non-null pointer is to a NUL-ended string in `text` (invariant 1),
            // alive while `argv` is, and nothing writes it here.
            let s = unsafe { core::ffi::CStr::from_ptr(p.cast()) };
            seen.push(s.to_str().unwrap_or_else(|e| panic!("ARGV: {e}")));
        }
        let strings: Vec<&str, Alloc<'_>> = Vec::from_iter_in(argv.strings(), erased.handle());
        assert_eq!(seen.as_slice(), strings.as_slice());
        assert_eq!(strings.as_slice(), &["ladder", "one", "", "three"]);
        println!("ARGV: argc 4, the C strings equal the hosted ones, argv[4] null");
    }

    /// A NUL inside an argument is refused by its index, never cut short.
    #[test]
    fn a_nul_in_an_argument_is_refused_by_index() {
        // The test's one allocator, made at its top: a frame, its memory from the OS.
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let rest = ["fine", "cut\0short"];
        match Argv::new_in("ladder", &rest, erased.handle()) {
            Err(error) => {
                assert_eq!(error, ArgvError::Nul { index: 2 });
                println!("ARGV NUL: {error}");
            }
            Ok(_) => panic!("ARGV NUL: an argument holding a NUL was taken"),
        }
    }
}
