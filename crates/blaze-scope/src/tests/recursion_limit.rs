//! Crate-level `#![recursion_limit = "N"]` support in `blaze-scope` (mirrors `image-webp`'s
//! `quick_error!` shape: `#![recursion_limit = "256"]` at the crate root allowing a >128-deep
//! recursive `macro_rules!` chain in a submodule to expand its enum, variants, and impls).

use std::fmt::Write as _;
use std::format;
use std::string::String;

use blaze_surface::{Namespace, ScopeError};

use super::entry::input;
use crate::crate_scope::{CrateScopeError, Ready, resolve_by_records};
use crate::graph::Sysroot;
use crate::names_record::{NamesRecord, Verified, encode};
use crate::resolve::{CrateNames, MadeNote, NamesError, NamesNote, SourceTexts, aligned};
use crate::task::Serial;

const WAIT: std::time::Duration = std::time::Duration::from_secs(60);

struct CrateFiles<'a>(&'a [(&'a str, &'a str)]);

impl<'a> SourceTexts<'a> for CrateFiles<'a> {
    fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
        if krate != 0 {
            return None;
        }
        self.0.iter().find(|(p, _)| *p == path).map(|(_, t)| *t)
    }
}

/// Builds a recursive `macro_rules!` invocation with `steps` peeling iterations (each step adds
/// one expansion depth and one enum variant), emitting `pub enum DecodingError` and an inherent
/// `impl DecodingError` at the base case — the `quick_error!` shape from `image-webp`.
fn quick_peel_body(steps: usize) -> String {
    let mut s = String::from(
        "macro_rules! quick_peel {\n\
         \t(@acc [$($v:ident),*] $head:ident $($rest:ident)*) => {\n\
         \t\tquick_peel!(@acc [$($v,)* $head] $($rest)*);\n\
         \t};\n\
         \t(@acc [$($v:ident),*]) => {\n\
         \t\tpub enum DecodingError {\n\
         \t\t\t$($v,)*\n\
         \t\t}\n\
         \t\timpl DecodingError {\n\
         \t\t\tpub fn tag(&self) -> u32 { 0 }\n\
         \t\t}\n\
         \t};\n\
         }\n\
         quick_peel!(@acc []",
    );
    for i in 0..steps {
        let _ = write!(s, " V{i}");
    }
    s.push_str(");\n");
    s
}

fn recursion_limit_errors(names: &CrateNames<'_>) -> usize {
    names
        .done
        .values()
        .flat_map(|d| d.scope.errors.iter())
        .filter(|e| matches!(e, ScopeError::RecursionLimit { .. }))
        .count()
}

fn try_solve_one<'a>(
    files: &'a [(&'a str, &'a str)],
    bump: blaze_own::Bump<'a>,
) -> Result<&'a CrateNames<'a>, (usize, CrateScopeError<'a>)> {
    let texts = CrateFiles(files);
    let crates = [input("k", "/k/lib.rs", &[])];
    let (solved, _) = resolve_by_records(&crates, Sysroot::default(), &texts, &Serial, &Ready::serial(WAIT), bump)?;
    Ok(solved[0].names)
}

fn solve_one<'a>(files: &'a [(&'a str, &'a str)], bump: blaze_own::Bump<'a>) -> &'a CrateNames<'a> {
    try_solve_one(files, bump).unwrap_or_else(|e| panic!("resolve_by_records failed: {e:?}"))
}

#[test]
fn crate_root_recursion_limit_unblocks_deep_macro_chain() {
    let peel = quick_peel_body(135);
    let default_root = "#![no_core]\npub use self::decoder::DecodingError;\nmod decoder;\n";
    let raised_root = "#![no_core]\n#![recursion_limit = \"256\"]\npub use self::decoder::DecodingError;\nmod decoder;\n";

    // Default limit (128): depth-135 macro chain fails with RecursionLimit and DecodingError is not emitted.
    let frame_default = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased_default = blaze_own::Erased::new(&frame_default);
    let default_files = [("/k/lib.rs", default_root), ("/k/decoder.rs", peel.as_str())];
    let names_default = solve_one(&default_files, erased_default.bump());
    assert_eq!(names_default.root_attrs.recursion_limit, u32::from(crate::RECURSION_LIMIT));
    assert_eq!(names_default.crate_root.head.recursion_limit, u32::from(crate::RECURSION_LIMIT));
    assert!(
        recursion_limit_errors(names_default) >= 1,
        "depth-135 chain must hit RecursionLimit at default 128"
    );
    let default_root_scope = &names_default.done.get(&names_default.root).expect("root done").scope;
    assert!(
        !default_root_scope.bindings.iter().any(|b| b.ns == Namespace::Types && b.name == "DecodingError"),
        "DecodingError must not resolve when macro expansion hits RecursionLimit"
    );

    // Raised limit (#![recursion_limit = "256"]): depth-135 chain succeeds, resolving enum, variants, and impl.
    let frame_raised = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased_raised = blaze_own::Erased::new(&frame_raised);
    let bump = erased_raised.bump();
    let raised_files = [("/k/lib.rs", raised_root), ("/k/decoder.rs", peel.as_str())];
    let names_raised = solve_one(&raised_files, bump);
    assert_eq!(names_raised.root_attrs.recursion_limit, 256);
    assert_eq!(names_raised.crate_root.head.recursion_limit, 256);
    assert_eq!(recursion_limit_errors(names_raised), 0, "no RecursionLimit errors with #![recursion_limit = \"256\"]");

    let raised_root_scope = &names_raised.done.get(&names_raised.root).expect("root done").scope;
    let decoding_err = raised_root_scope
        .bindings
        .iter()
        .find(|b| b.ns == Namespace::Types && b.name == "DecodingError")
        .expect("pub use self::decoder::DecodingError must resolve");
    let variants = names_raised
        .tables
        .enums
        .get(&decoding_err.binding.def.key)
        .expect("DecodingError variants must be recorded");
    assert_eq!(variants.len(), 135, "all 135 enum variants must be emitted");
    assert!(!names_raised.tables.impls.is_empty(), "inherent impl DecodingError must be recorded");

    // NamesRecord round-trips CrateRootHead::recursion_limit.
    let encoded = encode(names_raised, bump.alloc()).expect("encode NamesRecord");
    let verified = Verified::check(aligned(&encoded, bump)).expect("verify NamesRecord");
    let record = NamesRecord::open(verified);
    assert_eq!(record.crate_root().head().recursion_limit, 256);
}

#[test]
fn submodule_recursion_limit_does_not_override_crate_root() {
    let peel = quick_peel_body(135);

    // Out-of-line submodule with `#![recursion_limit = "256"]`.
    let root_out_of_line = "#![no_core]\nmod decoder;\n";
    let sub_out_of_line = format!("#![recursion_limit = \"256\"]\n{peel}");
    let frame1 = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased1 = blaze_own::Erased::new(&frame1);
    let files1 = [("/k/lib.rs", root_out_of_line), ("/k/decoder.rs", sub_out_of_line.as_str())];
    let names1 = solve_one(&files1, erased1.bump());
    assert_eq!(names1.root_attrs.recursion_limit, u32::from(crate::RECURSION_LIMIT));
    assert!(
        recursion_limit_errors(names1) >= 1,
        "out-of-line submodule #![recursion_limit] must not raise crate limit"
    );

    // Inline submodule with `#![recursion_limit = "256"]`.
    let root_inline = format!("#![no_core]\nmod decoder {{\n#![recursion_limit = \"256\"]\n{peel}\n}}\n");
    let frame2 = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased2 = blaze_own::Erased::new(&frame2);
    let files2 = [("/k/lib.rs", root_inline.as_str())];
    let names2 = solve_one(&files2, erased2.bump());
    assert_eq!(names2.root_attrs.recursion_limit, u32::from(crate::RECURSION_LIMIT));
    assert!(
        recursion_limit_errors(names2) >= 1,
        "inline submodule #![recursion_limit] must not raise crate limit"
    );
}

#[test]
fn cfg_attr_recursion_limit_respects_predicate() {
    let peel = quick_peel_body(135);

    let root_on = format!("#![no_core]\n#![cfg_attr(all(), recursion_limit = \"256\")]\n{peel}");
    let frame_on = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased_on = blaze_own::Erased::new(&frame_on);
    let files_on = [("/k/lib.rs", root_on.as_str())];
    let names_on = solve_one(&files_on, erased_on.bump());
    assert_eq!(names_on.root_attrs.recursion_limit, 256);
    assert_eq!(recursion_limit_errors(names_on), 0, "true cfg_attr must raise recursion limit");

    let root_off = format!("#![no_core]\n#![cfg_attr(any(), recursion_limit = \"256\")]\n{peel}");
    let frame_off = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased_off = blaze_own::Erased::new(&frame_off);
    let files_off = [("/k/lib.rs", root_off.as_str())];
    let names_off = solve_one(&files_off, erased_off.bump());
    assert_eq!(names_off.root_attrs.recursion_limit, u32::from(crate::RECURSION_LIMIT));
    assert!(recursion_limit_errors(names_off) >= 1, "false cfg_attr must not raise recursion limit");
}

#[test]
fn large_recursion_limit_above_4096_is_preserved() {
    let root = "#![no_core]\n#![recursion_limit = \"5000\"]\npub struct S;\n";
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let bump = erased.bump();
    let files = [("/k/lib.rs", root)];
    let names = solve_one(&files, bump);
    assert_eq!(names.root_attrs.recursion_limit, 5000, "limit 5000 must not be clamped to 4096");
    assert_eq!(names.crate_root.head.recursion_limit, 5000);

    let encoded = encode(names, bump.alloc()).expect("encode NamesRecord");
    let verified = Verified::check(aligned(&encoded, bump)).expect("verify NamesRecord");
    let record = NamesRecord::open(verified);
    assert_eq!(record.crate_root().head().recursion_limit, 5000);
}

#[test]
fn malformed_recursion_limit_is_rejected_with_typed_error() {
    let invalids = [
        "#![recursion_limit = \"abc\"]",
        "#![recursion_limit = \"\"]",
        "#![recursion_limit = \"-5\"]",
        "#![recursion_limit = \"+256\"]",
        "#![recursion_limit = \"256 \"]",
        "#![recursion_limit = \"1_024\"]",
        "#![recursion_limit = true]",
        "#![recursion_limit = 256]",
        "#![recursion_limit(256)]",
        "#![recursion_limit]",
        "#![recursion_limit = \"99999999999999999999999999999\"]",
        "#![recursion_limit = \"abc\"]\n#![recursion_limit = \"256\"]",
        "#![cfg_attr(all(), recursion_limit = \"abc\")]",
    ];
    for attr in invalids {
        let root = format!("#![no_core]\n{attr}\npub struct S;\n");
        let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
        let erased = blaze_own::Erased::new(&frame);
        let files = [("/k/lib.rs", root.as_str())];
        let err = try_solve_one(&files, erased.bump()).expect_err("malformed recursion_limit must fail");
        assert!(
            matches!(err, (0, CrateScopeError::Names(NamesError::InvalidRecursionLimit { krate: "k", .. }))),
            "attr `{attr}` must return NamesError::InvalidRecursionLimit, got {err:?}"
        );
    }

    // Inactive cfg_attr and attributes after a false `#![cfg(..)]` must NOT error.
    let inactive_cases = [
        "#![no_core]\n#![cfg_attr(any(), recursion_limit = \"abc\")]\npub struct S;\n",
        "#![no_core]\n#![cfg(any())]\n#![recursion_limit = \"abc\"]\npub struct S;\n",
    ];
    for root in inactive_cases {
        let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
        let erased = blaze_own::Erased::new(&frame);
        let files = [("/k/lib.rs", root)];
        let names = solve_one(&files, erased.bump());
        assert_eq!(names.root_attrs.recursion_limit, u32::from(crate::RECURSION_LIMIT));
    }
}

#[test]
fn zero_recursion_limit_rejects_macros_but_preserves_lexical_block_depth() {
    // `#![recursion_limit = "0"]` rejects even a depth-0 macro call.
    let zero_macro = "#![no_core]\n#![recursion_limit = \"0\"]\nmacro_rules! id { () => { pub struct S; }; }\nid!();\n";
    let frame_zero = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased_zero = blaze_own::Erased::new(&frame_zero);
    let files_zero = [("/k/lib.rs", zero_macro)];
    let names_zero = solve_one(&files_zero, erased_zero.bump());
    assert_eq!(names_zero.root_attrs.recursion_limit, 0);
    assert_eq!(recursion_limit_errors(names_zero), 1, "limit 0 must reject depth-0 call");

    // In a macro-free crate with `#![recursion_limit = "0"]`, lexical block nesting past 64
    // (here 70 nested `fn` bodies each with a local `struct` + `impl`) must still resolve cleanly
    // without hitting `MadeNote::TooMany` (lexical block depth floor remains 192).
    const NESTING: usize = 70;
    let mut nested_blocks = String::from("#![no_core]\n#![recursion_limit = \"0\"]\n");
    for i in 0..NESTING {
        let _ = writeln!(nested_blocks, "fn f{i}() {{ struct S{i}; impl S{i} {{}}");
    }
    for _ in 0..NESTING {
        nested_blocks.push_str("}\n");
    }
    let frame_blocks = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased_blocks = blaze_own::Erased::new(&frame_blocks);
    let files_blocks = [("/k/lib.rs", nested_blocks.as_str())];
    let names_blocks = solve_one(&files_blocks, erased_blocks.bump());
    assert_eq!(names_blocks.root_attrs.recursion_limit, 0);
    assert!(
        !names_blocks.notes.iter().any(|n| matches!(n, NamesNote::Made { note: MadeNote::TooMany, .. })),
        "70 nested lexical blocks must not hit MadeNote::TooMany under #![recursion_limit = \"0\"]: {:?}",
        names_blocks.notes
    );
    assert_eq!(
        names_blocks.tables.impls.len(),
        NESTING,
        "all {NESTING} nested block-local impls must be recorded"
    );
}
