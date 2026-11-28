//! End-to-end type-checking, borrow-checking, and native execution for `format_args!` width and
//! precision arguments (`FmtTrait::Count`) with deref-coercion (`&arg -> &usize`).
//!
//! Matches `rustc`'s `Argument::from_usize(&arg)`: `usize`, unsuffixed integer literals, `&usize`,
//! `&&usize`, `&mut usize`, `Box<usize>`, and custom `Deref<Target = usize>` all deref-coerce
//! cleanly, while non-`usize` types (`i32`, `&i32`, `&str`) fail with E0308 and borrow conflicts
//! on the count argument still fail borrow checking.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const EDITION: &str = "2024";

fn shelf() -> String {
    let home = std::env::var("HOME").expect("HOME is set");
    format!("{home}/.blaze/v2/toolchain")
}

fn check_project(text: &str) -> Vec<(String, bool, Vec<String>, Option<String>, Vec<String>)> {
    let heap = blaze_own::testing::heap();
    let shelf = shelf();
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("toolchain at {shelf}: {e}"));
    let files = [("/fmt-count/src/main.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("known edition");
    let user = UserCrate {
        name: "fmt_count",
        dir: "/fmt-count/",
        root: "/fmt-count/src/main.rs",
        files: &files,
        edition,
        panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("inputs: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, heap.alloc()).unwrap_or_else(|e| panic!("front2: {e}"));
    let bodies = world.bodies(heap.alloc());
    let mut out = Vec::new();
    for (body, _) in bodies {
        let name = match body {
            BodyRef::Free(k) => names.item_text(k).and_then(|t| t.name).unwrap_or("?").to_string(),
            BodyRef::Member { name, .. } => name.to_string(),
        };
        let (types, borrows) = world.check_borrows(body, heap.alloc());
        let type_diags: Vec<String> = types.diagnostics.iter().map(|d| d.code.to_string()).collect();
        let borrow_findings: Vec<String> = borrows.findings.iter().map(|d| d.code.to_string()).collect();
        let refused = borrows.refused.map(|r| format!("{r:?}"));
        out.push((name, types.is_clean(), type_diags, refused, borrow_findings));
    }
    out
}

#[test]
fn format_args_count_deref_typeck_and_borrowck() {
    const TEXT: &str = r#"use std::fmt;
use std::ops::Deref;

struct Wrap(usize);
impl Deref for Wrap {
    type Target = usize;
    fn deref(&self) -> &usize {
        &self.0
    }
}

struct ParseError {
    message: String,
    line: usize,
    column: usize,
    line_text: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { message, line, column, line_text } = self;
        let gutter = line.to_string().len();
        writeln!(f, "{message}: {line_text}")?;
        write!(f, "{:gutter$} | {:>column$}", "", "^")
    }
}

fn ok_all_count_forms(cond: bool) -> String {
    let unsuffixed = 4;
    let plain: usize = 4;
    let r: &usize = &plain;
    let rr: &&usize = &r;
    let mut m: usize = 4;
    let mr: &mut usize = &mut m;
    let boxed = Box::new(4usize);
    let custom = Wrap(4);
    let chosen = if cond { r } else { &plain };
    let mut out = String::new();
    out.push_str(&format!("{:unsuffixed$}", "a"));
    out.push_str(&format!("{:>r$}", "^"));
    out.push_str(&format!("{:>rr$}", "^"));
    out.push_str(&format!("{:>mr$}", "^"));
    out.push_str(&format!("{:>boxed$}", "^"));
    out.push_str(&format!("{:>custom$}", "^"));
    out.push_str(&format!("{:>chosen$}", "^"));
    out.push_str(&format!("{:.r$}", "abcdef"));
    out.push_str(&format!("{:.*}", r, "abcdef"));
    out
}

fn bad_i32() -> String {
    let w: i32 = 4;
    format!("{:w$}", "")
}

fn bad_ref_i32() -> String {
    let w: &i32 = &4;
    format!("{:w$}", "")
}

fn bad_str() -> String {
    let s: &str = "4";
    format!("{:s$}", "")
}

fn bad_borrow_conflict_value() -> String {
    let mut w = 4usize;
    let r = &mut w;
    format!("{:w$}", r)
}

fn bad_borrow_conflict_reborrow() -> String {
    let mut w = 4usize;
    let r = &mut w;
    format!("{:r$}", w)
}

fn main() {}
"#;

    let results = check_project(TEXT);
    let find = |n: &str| {
        results
            .iter()
            .find(|(name, ..)| name == n)
            .unwrap_or_else(|| panic!("missing fn `{n}` in {results:?}"))
    };

    for ok in ["fmt", "ok_all_count_forms"] {
        let (_, clean, tdiags, refused, bfindings) = find(ok);
        assert!(*clean, "`{ok}` typeck diags: {tdiags:?}");
        assert!(refused.is_none(), "`{ok}` borrowck refused: {refused:?}");
        assert!(bfindings.is_empty(), "`{ok}` borrowck findings: {bfindings:?}");
    }

    for bad_ty in ["bad_i32", "bad_ref_i32", "bad_str"] {
        let (_, clean, tdiags, _, _) = find(bad_ty);
        assert!(!*clean, "`{bad_ty}` must fail typeck");
        assert!(tdiags.iter().any(|c| c == "E0308"), "`{bad_ty}` expected E0308, got {tdiags:?}");
    }

    for bad_borrow in ["bad_borrow_conflict_value", "bad_borrow_conflict_reborrow"] {
        let (_, clean, tdiags, refused, bfindings) = find(bad_borrow);
        assert!(*clean, "`{bad_borrow}` must pass typeck, got {tdiags:?}");
        assert!(refused.is_none(), "`{bad_borrow}` borrowck refused: {refused:?}");
        assert!(!bfindings.is_empty(), "`{bad_borrow}` must fail borrowck");
    }
}

const NO_CORE_PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/prelude.rs");

#[test]
fn format_args_count_deref_runs_with_expected_output() {
    const MAIN: &str = r#"#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures, builtin_syntax)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

#[lang = "deref"]
pub trait Deref {
    type Target: PointeeSized;
    fn deref(&self) -> &Self::Target;
}

pub mod fmt {
    use super::*;

    pub trait Display {}

    impl Display for u32 {}

    #[lang = "format_arguments"]
    pub struct Arguments {
        pub template: *const u8,
        pub args: *const Argument,
    }

    #[lang = "format_argument"]
    #[repr(align(2))]
    pub struct Argument {
        pub value: usize,
        pub code: u64,
    }

    impl Argument {
        pub fn new_display<T: Display>(x: &T) -> Argument {
            Argument { value: unsafe { transmute(x) }, code: 1 }
        }

        pub fn from_usize(x: &usize) -> Argument {
            Argument { value: *x, code: 2 }
        }
    }
}

struct Wrap(usize);
impl Deref for Wrap {
    type Target = usize;
    fn deref(&self) -> &usize {
        &self.0
    }
}

struct ParseError {
    column: usize,
}

impl ParseError {
    fn count_val(&self) -> usize {
        let Self { column } = self;
        let x = 1u32;
        read_count(builtin # format_args("{:1$}", x, column))
    }
}

fn read_count(a: fmt::Arguments) -> usize {
    let g: &[fmt::Argument; 2] = unsafe { transmute(a.args) };
    if (*g)[0].code != 1 || (*g)[1].code != 2 {
        return 100;
    }
    (*g)[1].value
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let x = 5u32;
    let w_infer = 4;
    let c0 = read_count(builtin # format_args("{:1$}", x, w_infer));

    let w: usize = 4;
    let r: &usize = &w;
    let c1 = read_count(builtin # format_args("{:1$}", x, r));

    let rr: &&usize = &r;
    let c2 = read_count(builtin # format_args("{:1$}", x, rr));

    let mut mw: usize = 5;
    let mr: &mut usize = &mut mw;
    let c3 = read_count(builtin # format_args("{:1$}", x, mr));

    let d = Wrap(6);
    let c4 = read_count(builtin # format_args("{:1$}", x, d));

    let err = ParseError { column: 9 };
    let total = c0 + c1 + c2 + c3 + c4 + err.count_val();
    total as i32
}
"#;
    let files = [
        SourceFile { path: "src/main.rs", text: MAIN },
        SourceFile { path: "src/prelude.rs", text: NO_CORE_PRELUDE },
    ];
    let new = NewFront {
        package: "core",
        edition: EDITION,
        shelf: None,
        overflow: blaze_build::OverflowChecks::On,
    };
    let status = run_with(&files, "src/main.rs", None, FrontChoice::New(new), blaze_own::testing::heap().shared())
        .unwrap_or_else(|e| panic!("build/run failed: {e}"));
    // 4 (unsuffixed) + 4 (&usize) + 4 (&&usize) + 5 (&mut usize) + 6 (Deref) + 9 (ParseError) = 32.
    assert_eq!(status, 32);
}
