//! A fn's ABI is answered from its HEAD, also for a fn the program leaves out
//! (front2/program/head_abi.rs, the seam's `FrontEnd::fn_abi`).
//!
//! The Reference, "Functions": "Functions may qualify their signatures with an ABI"; the ABI is
//! part of the signature, not of the body. The program below gives trait `Tr` two provided
//! methods whose default bodies do not check ("Functions": "the type of the block must be
//! coercible to the declared return type"; `true` is a `bool`, never an `i32`), so the survey
//! leaves both out: `c`, declared `extern "C"`, and `r`, declared with no ABI (Rust). `main`
//! unsizes `&S` to `&dyn Tr`, so `Tr` has a table, which names both methods.
//!
//! What it asserts: neither is tabled (`fn_index` is `None`), `fn_abi(c)` is `Some(Abi::C)` and
//! `fn_abi(r)` is `Some(Abi::Rust)`. Before the fix both were `None` (the builder's `NoAbi`); a
//! fix that answered a left-out fn with Rust whatever its head says fails on `c`.
//!
//! Line it prints: `head abi: c C, r Rust`.
//!
//! It needs no toolchain: the program is `#![no_core]` (as tests/front2_callee_left_out.rs).
//! NOT RUN by the writer (a write-only lane).
#![feature(allocator_api)]

use std::alloc::Global;

use blaze_build::Abi;
use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::NewFront;
use blaze_ladder::seam::FrontEnd;
use blaze_typeck::PNode;

/// The lang items are modeled on the pinned library's own (library/core/src/marker.rs,
/// ops/unsize.rs), as tests/front2_callee_left_out.rs's program.
const PROGRAM: &str = r#"#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "pointee_sized"]
trait PointeeSized {}

#[lang = "meta_sized"]
trait MetaSized: PointeeSized {}

#[lang = "sized"]
trait Sized: MetaSized {}

#[lang = "legacy_receiver"]
trait LegacyReceiver: PointeeSized {}

impl<T: PointeeSized> LegacyReceiver for &T {}

#[lang = "copy"]
trait Copy {}

#[lang = "unsize"]
trait Unsize<T: PointeeSized>: PointeeSized {}

#[lang = "coerce_unsized"]
trait CoerceUnsized<T: PointeeSized> {}

impl<'a, 'b: 'a, T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<&'a U> for &'b T {}

#[lang = "dispatch_from_dyn"]
trait DispatchFromDyn<T> {}

impl<'a, T: PointeeSized + Unsize<U>, U: PointeeSized> DispatchFromDyn<&'a U> for &'a T {}

trait Tr {
    extern "C" fn c(&self) -> i32 {
        true
    }
    fn r(&self) -> i32 {
        true
    }
}

struct S;

impl Tr for S {}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let s = S;
    let _t: &dyn Tr = &s;
    0
}
"#;

#[test]
fn a_left_out_fn_has_the_abi_of_its_head() {
    let files = [SourceFile { path: "src/main.rs", text: PROGRAM }];
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
    let mut left_out = Vec::new_in(Global);
    let program = Front2Program::survey(&names, &frame, &files, &mut left_out, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("no survey: {e}"));
    let left: Vec<(String, String)> = left_out.iter().map(|(f, class)| (f.to_string(), class.to_string())).collect();
    println!("LEFT OUT: {left:?}");
    // `Tr`, the one principal the program's types name; its table lists `c` then `r`.
    let tys = program.types();
    let tr = tys
        .nodes()
        .find_map(|n| match *n {
            PNode::Dynamic(l) => blaze_typeck::object_principal(tys, l),
            _ => None,
        })
        .expect("`dyn Tr` among the program's types");
    let table = program.object_trait(tr).expect("`Tr`'s table");
    assert_eq!(table.methods.len(), 2, "`Tr`'s table holds `c` and `r`");
    let (c, r) = (table.methods[0].item, table.methods[1].item);
    assert_eq!(program.fn_index(c), None, "`c` is left out (its body does not check): {left:?}");
    assert_eq!(program.fn_index(r), None, "`r` is left out (its body does not check): {left:?}");
    let shown = |a: Option<Abi>| a.map_or_else(|| String::from("none"), |a| format!("{a:?}"));
    let line = format!("head abi: c {}, r {}", shown(program.fn_abi(c)), shown(program.fn_abi(r)));
    println!("{line}");
    assert_eq!(line, "head abi: c C, r Rust");
}
