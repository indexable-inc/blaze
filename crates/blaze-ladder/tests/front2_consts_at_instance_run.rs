//! Constants a head names that are first made while an INSTANCE is built, BUILT AND RUN on the
//! new front end. tempo's tests (2026-11-21, `blaze test -p tempo-hardfork` and ten more
//! packages) left 25 tests NOT RUN with "an answer an instance of `..` asked was not made:
//! Const { .. error: Nested { item .. } }" and 13 with ruint's "the type alias `U256` is
//! refused: ConstPath(\"nlimbs! ( 256 ): this caller has no constant evaluator\")": e.g.
//! chacha20 0.10.2 rng.rs, `const BUFFER_SIZE: usize = (BLOCK_WORDS * BUF_BLOCKS) as usize;`
//! read by `impl<R: Rounds, V: Variant> Generator for ChaChaCore<R, V> { type Output = [u32;
//! BUFFER_SIZE]; .. }`, normalized only when `BlockRng<ChaChaCore<..>>` is laid out at an
//! instance.
//!
//! The cause: the seam's `make_asked` (front2/program.rs and program/lazy/front_end.rs) made
//! what the instance questions asked (an impl's associated-type row, an impl header, a head)
//! with `Records { consts: None }`, so a head naming a constant met check.rs `eval_len` with no
//! evaluator: `ConstFail::Nested` for a named constant, "this caller has no constant evaluator"
//! for an anonymous one. The fix gives `make_asked` the evaluator the program keeps.
//!
//! The rules (the Reference): "Constant evaluation", "Const context": "Array type length
//! expressions" are const contexts; "Constant items": a constant's initializer may read other
//! constants ("Constants may refer to the address of other constants"; any constant expression
//! may name a constant item), to any depth, and the value is the same wherever it is used;
//! "Associated constants": a qualified path selects the impl's definition. A constant that needs
//! itself has no value: rustc's E0391 ("cycle detected when const-evaluating").
//! core's `ptr::read_unaligned` evaluates `const { assert!(mem::offset_of!(Unaligned::<T>, 0)
//! == 0); .. }` at each `T`: `offset_of!` in a constant is the layout's offset ("Type layout").
//!
//! Each test's program returns its value as the status; the twin is a refusal naming the cycle;
//! the control reads the same constant chain in a body checked before any instance (the path
//! that already had the evaluator), which ran before the fix too.
//!
//! The library is the async twin's (crates/blaze-cli/tests/ladder/front2/x0-async-01): the
//! programs are `#![no_core]`, built and run in this process with no toolchain. NOT YET RUN by
//! its writer (2026-11-21): written with the fix, compiled and run by the coordinator.

#![feature(allocator_api)]

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

const PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/prelude.rs");
const FUTURE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/future.rs");

const HEAD: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures, arbitrary_self_types, builtin_syntax)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod future;
mod prelude;
use prelude::*;
";

/// Builds and runs `HEAD` + `main` with the async twin's library: the status, or the refusal
/// on one line.
fn run(main: &str) -> Result<u8, String> {
    let text = [HEAD, main].concat();
    let files = [
        SourceFile { path: "src/main.rs", text: &text },
        SourceFile { path: "src/prelude.rs", text: PRELUDE },
        SourceFile { path: "src/future.rs", text: FUTURE },
    ];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| format!("{e}").replace('\n', " "))
}

/// chacha20's shape: a constant reading two constants (`(16u8 * 4u8) as usize` = 64), the
/// length of an impl's associated type, laid out only at the generic fns' instances at `Core`:
/// `size_of::<G::Output>()` is 64 words, and a struct holding it beside a `u8` (chacha20's
/// `BlockRng { core, results: R::Output, index }`) is 260 bytes, 65 words. 64 + 65 = 129.
/// Before the fix: "asked was not made: Const { .. error: Nested { .. shown: \"`..BUFFER_SIZE`\" } }".
#[test]
fn a_constant_chain_in_an_impls_associated_type_runs_at_an_instance() {
    let got = run("
const BLOCK_WORDS: u8 = 16;
const BUF_BLOCKS: u8 = 4;
const BUFFER_SIZE: usize = (BLOCK_WORDS * BUF_BLOCKS) as usize;

trait Generator {
    type Output;
}

struct Core;

impl Generator for Core {
    type Output = [u32; BUFFER_SIZE];
}

struct Buffered<G: Generator> {
    results: G::Output,
    index: u8,
}

fn output_words<G: Generator>() -> usize {
    size_of::<G::Output>() / size_of::<u32>()
}

fn buffered_words<G: Generator>() -> usize {
    size_of::<Buffered<G>>() / size_of::<u32>()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    (output_words::<Core>() + buffered_words::<Core>()) as i32
}
");
    println!("CONST CHAIN AT AN INSTANCE: {got:?}");
    assert_eq!(got, Ok(129), "BUFFER_SIZE = (16 * 4) as usize = 64 words; the holder 65 words");
}

/// The same chain read in a body checked before any instance (`main`'s own array type): 64.
/// Written as the control on the premise that this path always had the evaluator; it did not
/// (t80, 2026-11-21: "`main` did not lower: Convert { .. why: Some(NeedConst(..)) }"): a ROOT
/// was lowered by a bare `lower_fn` (front2/program.rs `make_from`) that returned the need as
/// the refusal, while every reached fn is lowered by `lower_made`, which evaluates the
/// constant and lowers again. The roots now go through `lower_made` too.
#[test]
fn the_same_constant_chain_in_a_checked_body_runs() {
    let got = run("
const BLOCK_WORDS: u8 = 16;
const BUF_BLOCKS: u8 = 4;
const BUFFER_SIZE: usize = (BLOCK_WORDS * BUF_BLOCKS) as usize;

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    (size_of::<[u32; BUFFER_SIZE]>() / size_of::<u32>()) as i32
}
");
    println!("CONST CHAIN IN A CHECKED BODY: {got:?}");
    assert_eq!(got, Ok(64));
}

/// The twin of the test above: the same written type in a fn `main` REACHES (lowered by
/// `lower_made` in reach.rs before the fix too), beside a second one in `main` itself, so the
/// root's path and the reached path are asked for the same constant: 64 + 64 = 128.
#[test]
fn the_same_constant_chain_in_a_root_and_a_reached_body_runs() {
    let got = run("
const BLOCK_WORDS: u8 = 16;
const BUF_BLOCKS: u8 = 4;
const BUFFER_SIZE: usize = (BLOCK_WORDS * BUF_BLOCKS) as usize;

fn reached() -> usize {
    size_of::<[u32; BUFFER_SIZE]>() / size_of::<u32>()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    (reached() + size_of::<[u8; BUFFER_SIZE]>()) as i32
}
");
    println!("CONST CHAIN IN A ROOT AND A REACHED BODY: {got:?}");
    assert_eq!(got, Ok(128));
}

/// The three other forms a head's constant takes, each an impl's associated type laid out at
/// an instance: an anonymous expression (`size_of::<u64>() * 2` = 16: ruint's
/// `{ nlimbs!(256) }` is one, after expansion), a qualified associated constant
/// (`<Wide as Width>::WORDS` = 3 * 4 = 12, selected from the impl) and an inherent one read
/// through a ground `Self` (`Self::LEN` = BYTES + 1 = 7, itself reading a constant).
/// 16 + 12 + 7 = 35.
#[test]
fn anonymous_qualified_and_inherent_constants_in_associated_types_run_at_an_instance() {
    let got = run("
const BYTES: usize = 6;

trait Width {
    const WORDS: usize;
}

struct Wide;

impl Width for Wide {
    const WORDS: usize = 3 * 4;
}

trait Generator {
    type Output;
}

struct ByExpr;
struct ByQualified;
struct ByInherent;

impl ByInherent {
    const LEN: usize = BYTES + 1;
}

impl Generator for ByExpr {
    type Output = [u8; size_of::<u64>() * 2];
}

impl Generator for ByQualified {
    type Output = [u8; <Wide as Width>::WORDS];
}

impl Generator for ByInherent {
    type Output = [u8; Self::LEN];
}

fn len<G: Generator>() -> usize {
    size_of::<G::Output>()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    (len::<ByExpr>() + len::<ByQualified>() + len::<ByInherent>()) as i32
}
");
    println!("ANONYMOUS, QUALIFIED, INHERENT AT AN INSTANCE: {got:?}");
    assert_eq!(got, Ok(35));
}

/// core's `read_unaligned` shape: an inline `const { .. }` at each instance reads
/// `offset_of!` of a `#[repr(C, packed)]` wrapper of `T` and its size (0 * 100 + 8 at `u64`,
/// 0 * 100 + 6 at `[u16; 3]`), and two `repr(C)` offsets in a const block: `Pair.c` at 8
/// (`a: u8` at 0, `b: u32` at 4, `c: u16` at 8) and the nested `Outer.p.c` at 4 + 8 = 12. The
/// nested one is also built at run time (blaze-build walk/strs.rs `offset_of`): the two must
/// agree (status 200 if not). 8 + 6 + 8 + 12 = 34.
/// Before the fix: "`offset_of!` (a field's offset from the layout) is not evaluated yet".
#[test]
fn offset_of_in_inline_consts_runs_at_each_instance() {
    let got = run("
#[repr(C, packed)]
struct Unaligned<T>(T);

#[repr(C)]
struct Pair {
    a: u8,
    b: u32,
    c: u16,
}

#[repr(C)]
struct Outer {
    x: u16,
    p: Pair,
}

fn unaligned<T>() -> usize {
    const { builtin # offset_of(Unaligned<T>, 0) * 100 + size_of::<Unaligned<T>>() }
}

fn at_run_time() -> usize {
    builtin # offset_of(Outer, p.c)
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let a = unaligned::<u64>() + unaligned::<[u16; 3]>();
    let b = const { builtin # offset_of(Pair, c) };
    let c = const { builtin # offset_of(Outer, p.c) };
    if c != at_run_time() {
        return 200;
    }
    (a + b + c) as i32
}
");
    println!("OFFSET_OF IN INLINE CONSTS: {got:?}");
    assert_eq!(got, Ok(34));
}

/// core's `read_unaligned` itself (the pinned library/core/src/ptr/mod.rs):
/// `const { assert!(mem::offset_of!(Unaligned::<T>, 0) == 0); assert!(size_of::<T>() ==
/// size_of::<Unaligned<T>>()); }` at each `T`, with the turbofish written in the type. tempo's
/// 38 NOT RUN lines (reports/tempo-tests/t0905) are this block at `u64`, `neon::uint32x4_t`
/// and `neon::uint64x2_t`: "the const block at expression 1 .. bytes 2688..2736" is the first
/// `assert!`. The cause, for this test and the one above: consteval's `offset_of` grounded the
/// body's WRITTEN type (`BodyView::types`) as a row of the record's table, another type.
/// `assert!` is not in the twin's library, so each check is a `bool` the block adds up, beside
/// the wrapper's size: at the 16-byte vector shape `[u32; 4]` 2 + 16 * 10 = 162, at `u64`
/// 2 + 8 * 10 = 82; 162 + 82 = 244. A failed check gives a sum that is not 244.
#[test]
fn read_unaligneds_const_block_runs_at_a_vector_and_a_word() {
    let got = run("
#[repr(C, packed)]
struct Unaligned<T>(T);

fn checks<T>() -> usize {
    const {
        (builtin # offset_of(Unaligned::<T>, 0) == 0) as usize
            + (size_of::<T>() == size_of::<Unaligned<T>>()) as usize
            + size_of::<Unaligned<T>>() * 10
    }
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    (checks::<[u32; 4]>() + checks::<u64>()) as i32
}
");
    println!("READ_UNALIGNED'S CONST BLOCK: {got:?}");
    assert_eq!(got, Ok(244));
}

/// A constant's layout question reaching an ADT whose field length names a constant: `SIZE`
/// names only `Outer`, whose field `Inner` is first laid out inside `SIZE`'s evaluation, and
/// `Inner`'s head needs `LEN` (2 + 3) while the evaluator is busy. The work list evaluates
/// `LEN` first and lays `Outer` out again (consts.rs `adt_row`): 5 + 1 = 6 bytes, all of
/// align 1. Before: "a layout it reads needs the row of ADT ..: .. NeedConst ..".
#[test]
fn a_field_length_constant_met_inside_a_layout_runs() {
    let got = run("
const LEN: usize = 2 + 3;

struct Inner {
    b: [u8; LEN],
}

struct Outer {
    inner: Inner,
    tag: u8,
}

const SIZE: usize = size_of::<Outer>();

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    SIZE as i32
}
");
    println!("FIELD LENGTH INSIDE A LAYOUT: {got:?}");
    assert_eq!(got, Ok(6));
}

/// The negative twin: two constants that need each other, the length of an impl's associated
/// type reached only at an instance. Rust refuses the program (E0391); blaze refuses the
/// instance by a typed refusal naming the cycle (consts.rs `ConstFail::Cycle`), never a value.
#[test]
fn a_constant_cycle_at_an_instance_is_refused_naming_the_cycle() {
    let got = run("
const A: usize = B + 1;
const B: usize = A + 1;

trait Generator {
    type Output;
}

struct Core;

impl Generator for Core {
    type Output = [u8; A];
}

fn len<G: Generator>() -> usize {
    size_of::<G::Output>()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    len::<Core>() as i32
}
");
    println!("CONST CYCLE AT AN INSTANCE: {got:?}");
    let why = got.expect_err("a constant that needs itself has no value (E0391)");
    assert!(why.to_lowercase().contains("cycle"), "the refusal names a cycle: {why}");
    assert!(!why.contains("Nested"), "the cycle is the evaluator's, not today's missing evaluator: {why}");
}
