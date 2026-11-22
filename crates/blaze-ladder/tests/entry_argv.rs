//! A program's entry is given the run's real command line (docs/v2/plan/design/run.md, "What a
//! program is given": the run spec holds "its arguments"), from ONE vector, crate::argv's
//! `Argv`: a C `main(argc, argv)` is called with its C form, and bstd's hosted entry answers its
//! `args` grant (the table's `arg` call) from the same strings. `argv[0]` is the program's name
//! (bstd `Args`: "the program's own name first"; the run parent's child gets `arg0: package`).
//!
//! The rules: C17 5.1.2.2.1, "Program startup": `int main(int argc, char *argv[])`; "the value
//! of argc shall be nonnegative"; "argv[argc] shall be a null pointer"; "If the value of argc is
//! greater than zero, the string pointed to by argv[0] represents the program name". The call:
//! AAPCS64 (arm64 macOS and arm64 Linux alike for two integer-class arguments): `argc` in
//! `w0`, `argv` in `x1`, the `int` result in `w0` (stub.rs `enter`).
//!
//! CONTROL (rule 8, one run): put run.rs `entry_shape`'s old line back (`if
//! !sig.inputs.is_empty() { return Err(shape("it takes parameters (argc and argv are not passed
//! yet)")); }` before the new match); `a_c_main_is_called_with_its_argc` then fails with
//! "C MAIN ARGC: refused: `main`: it takes parameters (argc and argv are not passed yet)".
//!
//! Prints:
//! `C MAIN ARGC: status 3` / `C MAIN ARGV: status 108` /
//! `HOSTED ARGS: the table's 3 arguments equal the C argv's` /
//! `C MAIN THREE: `main`: its parameters are neither none nor `(argc: i32, argv: *const *const c_char)`` /
//! `C MAIN NO VECTOR: `main` is a C `main` of `argc` and `argv`, and this run was given no argument vector to call it with`
//! and `test result: ok. 5 passed`.

#![feature(allocator_api)]

use blaze_ladder::argv::Argv;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, Given, NewFront, build_new, ran_built_given, run_with};

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

/// The lang items a `#![no_core]` program declares, as tests/stub_run.rs's.
const HEAD: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for i32 {}
impl Copy for u8 {}
";

/// A C `main` whose status is its `argc`.
const ARGC: &str = "
#[unsafe(no_mangle)]
pub extern \"C\" fn main(argc: i32, argv: *const *const u8) -> i32 {
    argc
}
";

/// A C `main` whose status is the first byte of `argv[0]`, the program's name.
const ARGV: &str = "
#[unsafe(no_mangle)]
pub extern \"C\" fn main(argc: i32, argv: *const *const u8) -> i32 {
    unsafe { **argv as i32 }
}
";

/// A `main` of three parameters: no start passes three.
const THREE: &str = "
#[unsafe(no_mangle)]
pub extern \"C\" fn main(argc: i32, argv: *const *const u8, envp: *const *const u8) -> i32 {
    argc
}
";

fn program(body: &str) -> String {
    format!("{HEAD}{body}")
}

/// Builds `text` and calls its entry with the command line `ladder` then `rest`: the status.
fn run_given(text: &str, rest: &[&str]) -> Result<u8, String> {
    let files = [SourceFile { path: "src/main.rs", text }];
    let heap = blaze_own::testing::heap();
    let mut argv = Argv::new_in(NEW.package, rest, heap.alloc()).map_err(|e| format!("{e}"))?;
    let built = build_new(&files, "src/main.rs", NEW, blaze_own::Alloc::global()).map_err(|e| format!("{e}"))?;
    // `argv` lives past the call: the C form points into it.
    let given = Given { host: None, args: Some(argv.c()) };
    let ran = ran_built_given(built, given, heap.shared()).map_err(|e| format!("{e}"));
    drop(argv);
    ran.map(|ran| ran.status)
}

/// `argc` counts the program's name and the two arguments: 3.
#[test]
fn a_c_main_is_called_with_its_argc() {
    let status = run_given(&program(ARGC), &["one", "two"]).unwrap_or_else(|e| panic!("C MAIN ARGC: refused: {e}"));
    println!("C MAIN ARGC: status {status}");
    assert_eq!(status, 3);
}

/// `argv[0]` is the program's name, `ladder`: its first byte is `l`, 108.
#[test]
fn a_c_main_reads_its_name_through_argv() {
    let status = run_given(&program(ARGV), &["one"]).unwrap_or_else(|e| panic!("C MAIN ARGV: refused: {e}"));
    println!("C MAIN ARGV: status {status}");
    assert_eq!(status, b'l');
}

/// The hosted start reads the same vector: the host's `arg` call answers each of `Argv`'s
/// strings in order, then `END`; each equals the C form's string at that index.
#[test]
fn the_hosted_args_see_the_same_vector() {
    use bstd::hosting::Hosting;
    use bstd::io::Io;
    use bstd::wire::code;

    let heap = blaze_own::testing::heap();
    let rest = ["one", "two words"];
    let mut argv = Argv::new_in("ladder", &rest, heap.alloc()).unwrap_or_else(|e| panic!("HOSTED ARGS: {e}"));
    let hosting = Hosting::new(Io::sim(1), argv.strings().map(String::from).collect(), Vec::new(), 4096, heap.shared());
    let table = hosting.table();
    let c = argv.c();
    let mut index = 0u64;
    loop {
        // SAFETY: the table's own `arg` with its own host, which lives (`hosting`).
        let answer = unsafe { (table.arg)(table.host, index) };
        if answer.code == code::END {
            break;
        }
        assert_eq!(answer.code, code::OK, "HOSTED ARGS: argument {index} refused");
        // SAFETY: an `OK` answer wrote `value` bytes at the start of the exchange, which holds
        // `exchange_len >= value` bytes (hosting.rs `answer_with`).
        let hosted = unsafe { core::slice::from_raw_parts(table.exchange, answer.value as usize) }.to_vec();
        assert!((index as usize) < c.argc as usize, "HOSTED ARGS: the host has more arguments than argc {}", c.argc);
        // SAFETY: `argv` holds `argc + 1` pointers, each below `argc` to a NUL-ended string,
        // alive while `argv` is (crate::argv invariants 1 to 3).
        let from_c = unsafe { core::ffi::CStr::from_ptr((*c.argv.as_ptr().add(index as usize)).cast()) }.to_bytes().to_vec();
        assert_eq!(hosted, from_c, "HOSTED ARGS: argument {index}");
        index += 1;
    }
    assert_eq!(index, c.argc as u64, "HOSTED ARGS: the host answered {index} arguments, argc is {}", c.argc);
    drop(table);
    drop(hosting);
    println!("HOSTED ARGS: the table's {index} arguments equal the C argv's");
}

/// Three parameters is no entry any start calls: refused by name, before anything is placed.
#[test]
fn a_c_main_of_three_parameters_is_refused_by_name() {
    let said = match run_given(&program(THREE), &[]) {
        Err(e) => e,
        Ok(status) => panic!("C MAIN THREE: ran to status {status}; no start passes three parameters"),
    };
    println!("C MAIN THREE: {said}");
    assert!(said.contains("its parameters are neither none nor `(argc: i32, argv: *const *const c_char)`"), "{said}");
}

/// A run given no argument vector refuses a C `main` of `argc` and `argv` by name (as a hosted
/// entry with no host table is `NoHost`), never calls it with a guessed one.
#[test]
fn a_c_main_given_no_vector_is_refused_by_name() {
    let text = program(ARGC);
    let files = [SourceFile { path: "src/main.rs", text: &text }];
    let said = match run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()) {
        Err(e) => format!("{e}"),
        Ok(status) => panic!("C MAIN NO VECTOR: ran to status {status} with no argument vector"),
    };
    println!("C MAIN NO VECTOR: {said}");
    assert!(said.contains("this run was given no argument vector to call it with"), "{said}");
}
