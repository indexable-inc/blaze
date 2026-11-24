//! A `#[track_caller]` call written by a derive or an attribute macro is placed where rustc
//! places it: at the outermost invocation's call site (rustc_const_eval
//! `location_triple_for_span`: "`span.ctxt().outer_expn().expansion_cause()`", then `lo()`).
//! An attribute call's site is its attribute from the `#` (rustc_expand `Invocation::span`:
//! `attr.span`); a derive's is its path in the `derive(..)` list (`path.span`); a function-like
//! call's is the call (`span`), and a call written inside one is placed at the outermost.
//!
//! The cause (tempo tests t0850, 132 NOT RUN, 2026-11-21): over records, `call_origin` was
//! refused by name (front2 source.rs `Records::call_origin`), so every fn a derive or an
//! attribute wrote had no site ("the world does not say where the expression is
//! (`Adts::caller_site`)"). The lazy front here is the records path (`LazyProgram::from_records`
//! over a loader of the solve's records): with that refusal back, every test below panics at
//! "has no source site". The whole front answered, but at the calling entry's first byte, a doc
//! comment above the derive: each test asserts that byte's position is not the answer (the
//! control), and the run tests return the line and column through the real build.
//!
//! The site a code record holds is (item, byte in the item's text, shown file): an edit above
//! the item moves the line, read from today's text (`cold_position`), and no record.

#![feature(allocator_api)]

use blaze_build::{OverflowChecks, Panic};
use blaze_id::ItemKey as LibKey;
use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::program::{Front2Program, lazy::LazyProgram};
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, Records};
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};
use blaze_ladder::seam::{FrontEnd, PositionGap};
use blaze_mem::Frame;
use blaze_own::Erased;
use blaze_scope::package_path::PackagePath;
use blaze_scope::resolve::{Names, aligned};
use blaze_scope::{items_record, names_record};
use blaze_typeck::{Expr, ExprId, ItemKey};

const ROOT: &str = "src/main.rs";

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: OverflowChecks::Off };

// ---- the records harness (as tests/front_equal_rules.rs: a loader of the solve's records) ----

#[derive(Clone)]
struct KeptCrate<'a> {
    key: blaze_id::CrateItemKey,
    names: &'a [u8],
    items: &'a [u8],
    path: &'a str,
    text: &'a str,
    expansions: Vec<(LibKey, &'a str, &'a [u8])>,
}

struct Kept<'a>(Vec<KeptCrate<'a>>);

impl<'a> Load<'a> for Kept<'a> {
    fn names(&self, krate: usize) -> Option<&'a [u8]> {
        Some(self.0.get(krate)?.names)
    }

    fn items(&self, krate: usize) -> Option<&'a [u8]> {
        Some(self.0.get(krate)?.items)
    }

    fn file(&self, krate: usize, path: PackagePath<'_>) -> Option<&'a str> {
        let c = self.0.get(krate)?;
        (path.as_str() == c.path).then_some(c.text)
    }

    fn expansion(&self, krate: usize, call: LibKey) -> Option<LoadedExpansion<'a>> {
        let row = self.0.get(krate)?.expansions.iter().find(|e| e.0 == call)?;
        let bytes = row.2;
        let origins = if bytes.is_empty() { (&[][..], &[][..]) } else { blaze_scope::expansion_origins::split(bytes).expect("the fixture expansion's origin column") };
        Some(LoadedExpansion { text: row.1, origins: Some(origins) })
    }
}

fn keep<'a>(names: &Names<'_>, frame: &'a Frame, alloc: blaze_own::Alloc<'a>) -> &'a Kept<'a> {
    let crates = names.crates().iter().map(|c| {
        let file = &c.files[0];
        KeptCrate {
            key: c.key,
            names: aligned(&names_record::encode(c, alloc).expect("names record"), blaze_own::testing::bump()),
            items: aligned(&items_record::encode_with(c, names, alloc).expect("items record"), blaze_own::testing::bump()),
            path: frame.alloc_str(file.package.as_str()),
            text: frame.alloc_str(file.text),
            expansions: c.expansions.iter().filter_map(|(call, e)| {
                use blaze_scope::expansion_origins::OriginsError;
                let made = e.made?;
                let origins: &[u8] = match made.origins {
                    Ok(bytes) => frame.alloc_slice_copy(bytes),
                    Err(OriginsError::Rewritten { .. } | OriginsError::Unplaced { .. }) => &[],
                    Err(error) => panic!("origins of expansion {call:x}: {error}"),
                };
                Some((*call, &*frame.alloc_str(made.text), origins))
            }).collect(),
        }
    }).collect();
    frame.alloc(Kept(crates))
}

fn on_checker_stack<R: Send + 'static>(run: impl FnOnce() -> R + Send + 'static) -> R {
    match std::thread::Builder::new().stack_size(256 << 20).spawn(run).expect("fixture thread").join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// The whole front over the solved names and the lazy front over the records written from them.
fn with_programs<R: Send + 'static>(text: &'static str, inspect: impl for<'n> FnOnce(&'n Names<'n>, &dyn FrontEnd, &mut dyn FrontEnd) -> R + Send + 'static) -> R {
    with_programs_kept(text, false, inspect)
}

/// The expansions of `kept` with no call part kept (a text whose origins are `Err` at its
/// writer, blaze-scope expansion_records.rs `write`, as core's shelf has some): what the
/// loader then serves is the empty part the store holds for them.
fn without_call_parts<'a>(kept: &Kept<'a>) -> Kept<'a> {
    Kept(kept.0.iter().map(|c| KeptCrate { expansions: c.expansions.iter().map(|&(call, text, _)| (call, text, &[][..])).collect(), ..c.clone() }).collect())
}

/// [`with_programs`], the records' expansions served without their call parts when
/// `drop_call_parts`.
fn with_programs_kept<R: Send + 'static>(text: &'static str, drop_call_parts: bool, inspect: impl for<'n> FnOnce(&'n Names<'n>, &dyn FrontEnd, &mut dyn FrontEnd) -> R + Send + 'static) -> R {
    let run = move || {
        let frame: &'static Frame = Box::leak(Box::new(Frame::new(blaze_mem::tag::Tag::Infer).expect("the fixture's frame")));
        let erased: &'static Erased<&'static Frame> = frame.alloc(Erased::new(frame));
        let a = erased.handle();
        let files = [SourceFile { path: ROOT, text }];
        let tc = Toolchain::none(a);
        let texts = ProgramTexts::new(&files, ROOT, NEW, &tc, Panic::Unwind).unwrap_or_else(|e| panic!("fixture texts: {e}"));
        let user = texts.user();
        let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("fixture inputs: {e}"));
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("fixture names: {e}"));
        let whole = Front2Program::new(&names, frame, &files, a).unwrap_or_else(|e| panic!("whole program: {e}"));
        let entry = whole.function(whole.entry().expect("the fixture exports main")).key;
        let c = names.krate(0);
        let kept = keep(&names, frame, a);
        let kept: &Kept<'_> = if drop_call_parts { frame.alloc(without_call_parts(kept)) } else { kept };
        let log = frame.alloc(Log::new(&["ladder"], a));
        let records = frame.alloc(Records::new(frame.alloc_slice_copy(&[c.key]), kept, log, frame, a));
        let mut lazy = LazyProgram::from_records(records, frame, &[0], Some(entry), a).unwrap_or_else(|e| panic!("lazy program: {e}"));
        lazy.decide_start(blaze_ladder::run::special::MakerStart::ByRule).unwrap_or_else(|e| panic!("lazy entry: {e}"));
        lazy.check_one(entry).unwrap_or_else(|e| panic!("checking main: {e}")).expect("main has a body");
        let result = inspect(&names, &whole, &mut lazy);
        assert!(records.errors().is_empty(), "fixture record reads: {:?}", records.errors());
        result
    };
    on_checker_stack(run)
}

/// Places a sealed site from the records of `names` and today's text `current` (a new command
/// over the old records, nothing of the program built).
fn cold_position(names: &Names<'_>, current: &'static str, item: ItemKey, offset: u32) -> Result<(u32, u32), PositionGap> {
    let frame = Box::leak(Box::new(Frame::new(blaze_mem::tag::Tag::Infer).expect("records reader's frame")));
    let erased = frame.alloc(Erased::new(&*frame));
    let a = erased.handle();
    let mut old = keep(names, frame, a).0.clone();
    old[0].text = current;
    let kept = frame.alloc(Kept(old));
    let keys = frame.alloc_slice_copy(&[names.krate(0).key]);
    let log = frame.alloc(Log::new(&["ladder"], a));
    let records = frame.alloc(Records::new(keys, kept, log, frame, a));
    let result = records.source_position(item, offset, a);
    if result.is_ok() {
        assert!(records.errors().is_empty(), "{:?}", records.errors());
    }
    result
}

// ---- the fixtures ----

const SITE_HEAD: &str = r#"#![feature(no_core, lang_items, macro_attr, macro_derive)]
#![no_core]
#![no_main]
#[lang = "sized"] trait Sized {}
#[lang = "copy"] trait Copy {}
impl Copy for u8 {}
#[track_caller] const fn tracked(_: u8) -> u8 { 7 }
macro_rules! Other { derive() ($($t:tt)*) => {}; }
"#;

/// The line and column (from 1; the column in chars) of the first byte of `written`, which
/// stands once in `text`.
fn line_col(text: &str, written: &str) -> (u32, u32) {
    assert_eq!(text.matches(written).count(), 1, "`{written}` stands once in the fixture");
    let prefix = &text[..text.find(written).expect("written in the fixture")];
    let line = 1 + prefix.bytes().filter(|&b| b == b'\n').count() as u32;
    let col = 1 + prefix.rsplit('\n').next().expect("a line").chars().count() as u32;
    (line, col)
}

/// The one fn `sample` the expansion writes has one call, of `tracked`. Over records and over
/// the whole front it is placed at `site`'s first byte (rustc's call site); the calling entry's
/// first byte, `entry` (the doc comment above the attributes: the answer before the fix), is
/// another place (the control); with three lines inserted before `insert_before`, the same
/// sealed site is three lines later, read from today's text over the old records.
fn check_expansion_site(decls: &str, site: &'static str, entry: &'static str, insert_before: &str) {
    check_expansion_site_calling(decls, "S::sample()", site, entry, insert_before)
}

/// [`check_expansion_site`] with `main` returning `call` (a `sample` written in a body is
/// reached through the body). Every `sample` is shown by a path with no block module in it
/// (item_shown.rs `shown_segments`).
fn check_expansion_site_calling(decls: &str, call: &str, site: &'static str, entry: &'static str, insert_before: &str) {
    let text = format!("{SITE_HEAD}{decls}\n#[unsafe(no_mangle)] pub extern \"C\" fn main() -> i32 {{ {call} as i32 }}");
    let mut moved = text.clone();
    moved.insert_str(text.find(insert_before).expect("edit anchor"), "\n\n\n");
    let text: &'static str = Box::leak(text.into_boxed_str());
    let moved: &'static str = Box::leak(moved.into_boxed_str());
    let expected = line_col(text, site);
    let old_answer = line_col(text, entry);
    assert_ne!(old_answer, expected, "the control: the entry's first byte is another place than the call site");
    let (moved_line, moved_col) = line_col(moved, site);
    assert_eq!((moved_line, moved_col), (expected.0 + 3, expected.1));
    let saved = with_programs(text, move |names, whole, lazy| {
        let mut owners = (0..whole.fn_count()).map(|i| whole.function(i))
            .filter(|f| f.body.is_some() && f.path.rsplit("::").next() == Some("sample"));
        let body = owners.next().expect("the expansion's `sample` has a live body");
        assert!(owners.next().is_none(), "one `sample`");
        let key = body.key;
        lazy.check_one(key).expect("check `sample`").expect("`sample` has a body");
        let site_in = |label: &str, front: &dyn FrontEnd| {
            let row = front.function(front.fn_index(key).expect("owner row"));
            let exprs = &row.body.expect("live caller").0.exprs;
            let mut calls = exprs.iter().enumerate().filter(|(_, e)| matches!(e, Expr::Call { .. }));
            let (i, _) = calls.next().expect("the call of `tracked`");
            assert!(calls.next().is_none(), "only the call of `tracked` is in `sample`");
            let s = front.caller_site(row.key, ExprId(i as u32)).unwrap_or_else(|gap| panic!("{label}: the call in `sample` has no source site: {gap}"));
            assert!(s.file.ends_with(ROOT), "{label}: shown path {}", s.file);
            let placed = front.source_position(s.item, s.offset);
            assert_eq!(placed, Ok(expected), "{label}: `{site}` is the call site");
            assert_ne!(placed, Ok(old_answer), "{label}: not the entry's first byte");
            (s.file.to_owned(), s.item, s.offset)
        };
        let saved = site_in("records", &*lazy);
        assert_eq!(site_in("whole", whole), saved, "both fronts seal one site");
        let named = lazy.item_named(key).expect("`sample` has a path");
        assert!(named.ends_with("::sample") && !named.contains("__blaze_block"), "shown with no block module: {named}");
        assert_eq!(cold_position(names, moved, saved.1, saved.2), Ok((moved_line, moved_col)), "the sealed site over today's text");
        saved
    });
    // A build after the edit seals the very same site: the record holds no line.
    let again = with_programs(moved, move |_, whole, lazy| {
        let key = (0..whole.fn_count()).map(|i| whole.function(i))
            .find(|f| f.body.is_some() && f.path.rsplit("::").next() == Some("sample")).expect("`sample`").key;
        lazy.check_one(key).expect("check `sample`").expect("`sample` has a body");
        let row = lazy.function(lazy.fn_index(key).expect("owner row"));
        let (i, _) = row.body.expect("live caller").0.exprs.iter().enumerate().find(|(_, e)| matches!(e, Expr::Call { .. })).expect("the call");
        let s = lazy.caller_site(row.key, ExprId(i as u32)).expect("the moved call's site");
        assert_eq!(lazy.source_position(s.item, s.offset), Ok((moved_line, moved_col)));
        (s.file.to_owned(), s.item, s.offset)
    });
    assert_eq!(again, saved, "an edit above the item seals the same site");
    println!("{site}: placed at {}:{} over records and the whole front, {}:{} after three lines above; entry's first byte {}:{} refused as the answer",
        expected.0, expected.1, moved_line, moved_col, old_answer.0, old_answer.1);
}

/// A derive: the site is the derive's path in its list, not the doc comment or the `#`.
#[test]
fn a_derived_fns_call_site_is_the_derive_path() {
    check_expansion_site(
        "macro_rules! Sampled { derive() ($(#[$m:meta])* struct $name:ident;) => { impl $name { fn sample() -> u8 { tracked(9) } } }; }\n\
         /// A doc comment above the derive.\n#[derive(Other,\n    Sampled)]\nstruct S;",
        "Sampled)]", "/// A doc comment", "/// A doc comment")
}

/// alloy-rlp-derive's shape: the impl in `const _: () = { .. };` (a block module,
/// `__blaze_block_0::<impl S>::sample`).
#[test]
fn a_derived_fn_in_an_anonymous_const_is_placed_at_the_derive_path() {
    check_expansion_site(
        "macro_rules! Sampled { derive() ($(#[$m:meta])* struct $name:ident;) => { const _: () = { impl $name { fn sample() -> u8 { tracked(9) } } }; }; }\n\
         /// A doc comment above the derive.\n#[derive(Other, Sampled)]\nstruct S;",
        "Sampled)]", "/// A doc comment", "/// A doc comment")
}

/// test_case's shape: an attribute that keeps its item and writes a fn next to it. The site
/// is the attribute from its `#`; the derive path list above it is not the call.
#[test]
fn an_attribute_written_fns_call_site_is_the_attribute() {
    check_expansion_site(
        "struct S;\nmacro_rules! case { attr() ($(#[$m:meta])* fn $name:ident() {}) => { fn $name() {} impl S { fn sample() -> u8 { tracked(10) } } }; }\n\
         /// A doc comment above the attribute.\n#[case]\nfn source() {}",
        "#[case]", "/// A doc comment", "/// A doc comment")
}

/// The twin of nesting: a derive written by a function-like macro is placed at the outermost
/// call (`wrap!()`), whose site is the call itself.
#[test]
fn a_derive_written_by_a_macro_is_placed_at_the_outermost_call() {
    check_expansion_site(
        "macro_rules! Sampled { derive() ($(#[$m:meta])* struct $name:ident;) => { impl $name { fn sample() -> u8 { tracked(9) } } }; }\n\
         macro_rules! wrap { () => { #[derive(Sampled)] struct S; } }\n/// A doc comment above the call.\nwrap!();",
        "wrap!();", "/// A doc comment", "/// A doc comment")
}

/// tempo-primitives' `ConfigurableAccountConfigTests::malformed_rlp_header_check` shape: an
/// attribute macro invoked inside `#[cfg_attr(..)]` (after a `#[derive(..)]`) that generates
/// a module containing a fn calling a `#[track_caller]` fn. Before the fix, `invoked_at` only
/// searched top-level `#[attr]` and `derive(..)` lists, refusing `#[cfg_attr(.., attr(..))]`
/// with `PositionGap::NoItem`. rustc places the `Location` at the inner attribute's path.
#[test]
fn an_attribute_macro_in_cfg_attr_generating_a_module_is_placed_at_the_attr_path() {
    check_expansion_site_calling(
        "macro_rules! add_tests { attr($($a:tt)*) ($(#[$m:meta])* struct $name:ident;) => {\n\
             struct $name;\n\
             mod tests { pub fn sample() -> u8 { super::tracked(11) } }\n\
         }; }\n\
         /// A doc comment above the struct.\n\
         #[derive(Other)]\n\
         #[cfg_attr(all(), add_tests(rlp))]\n\
         struct S;",
        "tests::sample()", "add_tests(rlp))]", "/// A doc comment", "/// A doc comment")
}

/// Multi-item and nested `#[cfg_attr(..)]`, with an inactive `#[cfg_attr(any(), ..)]` above
/// it: `invoked_at` evaluates each `cfg_attr` predicate against the crate's cfg and places
/// the site at the active inner attribute's path, skipping the inactive branch.
#[test]
fn a_nested_cfg_attr_attribute_macro_skips_inactive_branches() {
    check_expansion_site_calling(
        "mod codecs {\n\
             macro_rules! add_tests { attr($($a:tt)*) ($(#[$m:meta])* struct $name:ident;) => {\n\
                 struct $name;\n\
                 pub mod tests { pub fn sample() -> u8 { super::tracked(12) } }\n\
             }; }\n\
             pub(crate) use add_tests;\n\
         }\n\
         /// A doc comment above the struct.\n\
         #[cfg_attr(any(), codecs::add_tests(inactive))]\n\
         #[cfg_attr(all(), allow(unused), cfg_attr(all(), codecs::add_tests(active)))]\n\
         struct S;",
        "tests::sample()", "codecs::add_tests(active)))]", "/// A doc comment", "/// A doc comment")
}

/// A function-like macro generating `mod m { pub fn sample() { tracked(..); } }`, with a doc
/// comment above the call: placed at the macro call's path, past the doc comment.
#[test]
fn a_macro_generated_module_fn_is_placed_at_the_call_past_doc_comments() {
    check_expansion_site_calling(
        "macro_rules! make_mod { () => { mod m { pub fn sample() -> u8 { super::tracked(13) } } }; }\n\
         /// A doc comment above the call.\n\
         make_mod!();",
        "m::sample()", "make_mod!();", "/// A doc comment", "/// A doc comment")
}

/// Nested macro-in-macro generating `mod m { inner!(); }` -> `fn sample()`: placed at the
/// outermost call's path, past doc comments and outer attributes.
#[test]
fn a_nested_macro_generated_module_fn_is_placed_at_the_outermost_call() {
    check_expansion_site_calling(
        "macro_rules! inner { () => { pub fn sample() -> u8 { super::tracked(14) } }; }\n\
         macro_rules! outer { () => { mod m { inner!(); } }; }\n\
         /// A doc comment above the outer call.\n\
         #[allow(unused)]\n\
         outer!();",
        "m::sample()", "outer!();", "/// A doc comment", "/// A doc comment")
}

/// tempo-primitives' `Call` / `KeyAuthorization` / `TempoHeader` shape: a derive with an inert
/// helper attribute precedes a later derive and a `#[cfg_attr(.., attr(..))]`, so `blaze-scope`
/// stages the item through a `Keep` call (`macro_name == "derive"`). `Anchor::placement` must
/// look through the transparent keep pass to the real attribute/derive call instead of matching
/// `"derive"` (which either pointed at an earlier `#[derive]` or failed with `NoItem` when the
/// first derive was itself inside `#[cfg_attr(..)]`).
#[test]
fn a_cfg_attr_attribute_after_a_derive_and_helper_skips_the_keep_stage() {
    const TEXT: &str = concat!(
        "#![feature(no_core, lang_items, rustc_attrs, decl_macro, macro_attr, macro_derive)]\n",
        "#![no_core]\n#![no_main]\n#![allow(internal_features)]\n",
        "#[lang = \"sized\"] trait Sized {}\n#[lang = \"copy\"] trait Copy {}\nimpl Copy for u8 {}\n",
        "#[rustc_builtin_macro(Copy, attributes(helper))]\npub macro Copy($item:item) {}\n",
        "#[track_caller] const fn tracked(_: u8) -> u8 { 7 }\n",
        "macro_rules! Other { derive() ($($t:tt)*) => {}; }\n",
        "macro_rules! add_tests { attr($($a:tt)*) ($(#[$m:meta])* struct $name:ident;) => { struct $name; mod tests { pub fn sample() -> u8 { super::tracked(15) } } }; }\n",
        "/// A doc comment above the struct.\n",
        "#[cfg_attr(all(), derive(Copy))]\n#[helper]\n#[cfg_attr(all(), derive(Other))]\n#[cfg_attr(all(), add_tests(rlp))]\nstruct S;\n",
        "#[unsafe(no_mangle)] pub extern \"C\" fn main() -> i32 { tests::sample() as i32 }\n",
    );
    const MOVED: &str = concat!(
        "#![feature(no_core, lang_items, rustc_attrs, decl_macro, macro_attr, macro_derive)]\n",
        "#![no_core]\n#![no_main]\n#![allow(internal_features)]\n",
        "#[lang = \"sized\"] trait Sized {}\n#[lang = \"copy\"] trait Copy {}\nimpl Copy for u8 {}\n",
        "#[rustc_builtin_macro(Copy, attributes(helper))]\npub macro Copy($item:item) {}\n",
        "#[track_caller] const fn tracked(_: u8) -> u8 { 7 }\n",
        "macro_rules! Other { derive() ($($t:tt)*) => {}; }\n",
        "macro_rules! add_tests { attr($($a:tt)*) ($(#[$m:meta])* struct $name:ident;) => { struct $name; mod tests { pub fn sample() -> u8 { super::tracked(15) } } }; }\n",
        "\n\n\n/// A doc comment above the struct.\n",
        "#[cfg_attr(all(), derive(Copy))]\n#[helper]\n#[cfg_attr(all(), derive(Other))]\n#[cfg_attr(all(), add_tests(rlp))]\nstruct S;\n",
        "#[unsafe(no_mangle)] pub extern \"C\" fn main() -> i32 { tests::sample() as i32 }\n",
    );
    let expected = line_col(TEXT, "add_tests(rlp))]");
    let first_derive = line_col(TEXT, "derive(Copy))]");
    let (moved_line, moved_col) = line_col(MOVED, "add_tests(rlp))]");
    with_programs(TEXT, move |names, whole, lazy| {
        let key = (0..whole.fn_count())
            .map(|i| whole.function(i))
            .find(|f| f.body.is_some() && f.path.rsplit("::").next() == Some("sample"))
            .expect("`sample`")
            .key;
        lazy.check_one(key).expect("check `sample`").expect("`sample` has a body");
        for (label, front) in [("whole", whole as &dyn FrontEnd), ("records", &*lazy)] {
            let row = front.function(front.fn_index(key).expect("owner row"));
            let (i, _) = row.body.expect("live caller").0.exprs.iter().enumerate()
                .find(|(_, e)| matches!(e, Expr::Call { .. })).expect("the call of `tracked`");
            let site = front.caller_site(row.key, ExprId(i as u32))
                .unwrap_or_else(|gap| panic!("{label}: `sample` through keep stage has no site: {gap}"));
            let pos = front.source_position(site.item, site.offset);
            assert_eq!(pos, Ok(expected), "{label}: placed at `add_tests(rlp)`");
            assert_ne!(pos, Ok(first_derive), "{label}: not placed at the earlier derive");
            assert_eq!(cold_position(names, MOVED, site.item, site.offset), Ok((moved_line, moved_col)), "{label}: cold site after edit");
        }
    });
}

// ---- a call written in a block of a body (tempo t1855d, 2026-11-21: 50 NOT RUN) ----
//
// Before: a call written in a body's block is in the names library's module text of that
// body's blocks (`(body, "blocks")`, resolve.rs `make_blocks`), which no call wrote and which
// keeps no origins, so the walk out to the outermost call stopped there (`call_origin` of the
// fragment: no expansion) and every `#[track_caller]` call such a call's expansion writes had
// no site ("the world does not say where the expression is (`Adts::caller_site`)"): tempo's
// `#[derive(serde::Serialize)] struct BlockRef` in `fn serialize`, `#[derive(RlpDecodable)]
// struct WireSignature` in `<impl Decodable for ..>::decode`, rand's `const _: () = unsafe {
// impl_fill!(..) };`. Now the call's site in the fragment is carried back to the byte of the
// body it was copied from (seam_rules/local_position.rs `block_call_site`), and the body is
// placed as any item. With that step removed every row below panics at "has no source site".
// The edit above the body moves the line and not the sealed site; the doc comment above the
// call (the calling entry's first byte) is the control in each.

const IN_CONST: &str = "macro_rules! Sampled { derive() ($(#[$m:meta])* struct $name:ident;) => { const _: () = { impl $name { fn sample() -> u8 { tracked(9) } } }; }; }\n";

/// serde's shape in tempo's `serde_sealed_or_recovered_block::serialize`: a derive on a struct
/// declared in a fn's body, the impl in the derive's `const _: () = { .. };`.
#[test]
fn a_derive_in_a_fn_body_is_placed_at_the_derive_path() {
    check_expansion_site_calling(
        &format!("{IN_CONST}fn outer() -> u8 {{\n    /// A doc comment above the derive.\n    #[derive(Other, Sampled)]\n    struct S;\n    S::sample()\n}}"),
        "outer()", "Sampled)]", "/// A doc comment", "fn outer")
}

/// The twin: the derive writes its impl bare, with no `const _` around it.
#[test]
fn a_bare_derive_in_a_fn_body_is_placed_at_the_derive_path() {
    check_expansion_site_calling(
        "macro_rules! Sampled { derive() ($(#[$m:meta])* struct $name:ident;) => { impl $name { fn sample() -> u8 { tracked(9) } } }; }\n\
         fn outer() -> u8 {\n    /// A doc comment above the derive.\n    #[derive(Other, Sampled)]\n    struct S;\n    S::sample()\n}",
        "outer()", "Sampled)]", "/// A doc comment", "fn outer")
}

/// The body is a member (tempo's `#[derive(alloy_rlp::RlpDecodable)] struct WireSignature` in
/// `<impl Decodable for ConfigurableAccountSignature>::decode`): placed through its impl.
#[test]
fn a_derive_in_a_member_fns_body_is_placed_at_the_derive_path() {
    check_expansion_site_calling(
        &format!("{IN_CONST}struct T;\nimpl T {{\n    fn outer() -> u8 {{\n        /// A doc comment above the derive.\n        #[derive(Other, Sampled)]\n        struct S;\n        S::sample()\n    }}\n}}"),
        "T::outer()", "Sampled)]", "/// A doc comment", "struct T;")
}

/// rand's shape (rng.rs `const _: () = unsafe { impl_fill!(u16, u32, u64, u128,) };`): a
/// function-like call in a const's block whose expansion is a block of statement calls, the
/// impl two calls in. rustc places it at the outermost call, the one the const's block holds.
/// The call is the block's tail, so the block's module text writes it with a `;` of its own
/// (blaze-scope expanded_blocks.rs `render`): with that `;` taken as the body's again
/// (local_position.rs `written_range`), this row has no site over records (the 19:41 run).
#[test]
fn a_statement_call_in_a_consts_block_is_placed_at_the_call() {
    check_expansion_site(
        "struct S;\nmacro_rules! fill {\n    () => {};\n    (@one $t:ident) => {{ impl $t { fn sample() -> u8 { tracked(9) } } }};\n    ($t:ident, $($tt:ident,)*) => {{ fill!(@one $t); fill!($($tt,)*); }};\n}\n\
         /// A doc comment above the const.\nconst _: () = unsafe { fill!(S,) };",
        "fill!(S,)", "/// A doc comment", "/// A doc comment")
}

/// The twin: the same call as a statement, its `;` written in the const's block.
#[test]
fn a_statement_call_with_its_semicolon_in_a_consts_block_is_placed_at_the_call() {
    check_expansion_site(
        "struct S;\nmacro_rules! fill {\n    () => {};\n    (@one $t:ident) => {{ impl $t { fn sample() -> u8 { tracked(9) } } }};\n    ($t:ident, $($tt:ident,)*) => {{ fill!(@one $t); fill!($($tt,)*); }};\n}\n\
         /// A doc comment above the const.\nconst _: () = { fill!(S,); };",
        "fill!(S,);", "/// A doc comment", "/// A doc comment")
}

// ---- an expansion record with no call part (tempo a1425) ----

const NO_CALL_PART: &str = "macro_rules! Sampled { derive() ($(#[$m:meta])* struct $name:ident;) => { impl $name { fn sample() -> u8 { tracked(9) } } }; }\n\
    #[derive(Sampled)]\nstruct S;\n";

fn no_call_part_text() -> &'static str {
    Box::leak(format!("{SITE_HEAD}{NO_CALL_PART}#[unsafe(no_mangle)] pub extern \"C\" fn main() -> i32 {{ S::sample() as i32 }}").into_boxed_str())
}

/// Before (tempo a1425, 2026-11-21): an expansion whose record keeps no call part was filed in
/// the records' log (`call_text`: "the expansion's record keeps no call part"), which voids every
/// instance made over them, and the suite stopped. Now both questions answer it typed and the
/// log stays empty: `call_origin` is `NoCallPart`, `call_text` no calling text. TWIN: with its
/// call part kept, the same call is answered (`call_text` the file, `call_origin` its view).
/// PLANTED: the gap restored in `call_text` fails the empty-log assertion here.
#[test]
fn an_expansion_record_without_a_call_part_is_a_typed_answer_and_no_records_error() {
    use blaze_ladder::front2::expansion_origin::Unmapped;
    use blaze_ladder::front2::source::NamesSource;
    let text = no_call_part_text();
    on_checker_stack(move || {
        let frame: &'static Frame = Box::leak(Box::new(Frame::new(blaze_mem::tag::Tag::Infer).expect("the fixture's frame")));
        let erased: &'static Erased<&'static Frame> = frame.alloc(Erased::new(frame));
        let a = erased.handle();
        let files = [SourceFile { path: ROOT, text }];
        let tc = Toolchain::none(a);
        let texts = ProgramTexts::new(&files, ROOT, NEW, &tc, Panic::Unwind).unwrap_or_else(|e| panic!("fixture texts: {e}"));
        let user = texts.user();
        let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("fixture inputs: {e}"));
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("fixture names: {e}"));
        let calls: Vec<LibKey> = names.krate(0).expansions.iter().filter(|(_, e)| e.made.is_some()).map(|(call, _)| *call).collect();
        assert!(!calls.is_empty(), "the derive made a text");
        let kept = keep(&names, frame, a);
        let dropped: &'static Kept<'static> = frame.alloc(without_call_parts(kept));
        for (drop, served) in [(true, dropped), (false, kept)] {
            let log = frame.alloc(Log::new(&["ladder"], a));
            let records: &'static Records<'static> = frame.alloc(Records::new(frame.alloc_slice_copy(&[names.krate(0).key]), served, log, frame, a));
            for &call in &calls {
                let origin = records.call_origin(0, call).map(|_| ());
                let calling = records.call_text(0, call);
                if drop {
                    assert_eq!(origin, Err(Unmapped::NoCallPart { call }), "no call part: typed");
                    assert_eq!(calling, None, "no call part: no calling text");
                } else {
                    assert_eq!(origin, Ok(()), "twin: the kept call part is answered");
                    assert_eq!(calling, Some(blaze_scope::resolve::At::File(0)), "twin: the derive stands in the root file");
                }
            }
            assert!(records.errors().is_empty(), "drop {drop}: no records error, so no instance is void: {:?}", records.errors());
        }
    })
}

/// The instance a body of such a text needs is still made (the lazy front checks `sample`, and
/// [`with_programs`] asserts the records' log empty): only the `#[track_caller]` location
/// through that call is unplaced, by name at its use (blaze-build `Adts::caller_site`).
/// CONTROL: with the call part kept, the same call is placed at the derive path.
#[test]
fn a_body_written_by_an_expansion_without_a_call_part_is_made_and_only_its_location_unplaced() {
    let text = no_call_part_text();
    let (line, col) = line_col(text, "Sampled)]");
    let site = |drop| with_programs_kept(text, drop, |_, whole, lazy| {
        let key = (0..whole.fn_count()).map(|i| whole.function(i))
            .find(|f| f.body.is_some() && f.path.rsplit("::").next() == Some("sample")).expect("`sample`").key;
        lazy.check_one(key).expect("`sample` is checked without the call part").expect("`sample` has a body");
        let row = lazy.function(lazy.fn_index(key).expect("owner row"));
        let (i, _) = row.body.expect("live caller").0.exprs.iter().enumerate().find(|(_, e)| matches!(e, Expr::Call { .. })).expect("the call");
        lazy.caller_site(row.key, ExprId(i as u32)).ok().map(|s| lazy.source_position(s.item, s.offset))
    });
    assert_eq!(site(true), None, "no call part: only the location is unplaced");
    assert_eq!(site(false), Some(Ok((line, col))), "control: with the call part, the derive path");
}

// ---- the run test: the `Location` the built program reads ----

const RUN_HEAD: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs, macro_derive)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u32 {}

#[lang = \"panic_location\"]
struct Location<'a> {
    file: &'a str,
    line: u32,
    col: u32,
}

#[rustc_intrinsic]
const fn caller_location() -> &'static Location<'static>;

#[track_caller]
fn here() -> &'static Location<'static> {
    caller_location()
}

macro_rules! Placed {
    derive() ($(#[$m:meta])* struct $name:ident;) => {
        const _: () = {
            impl $name {
                fn line() -> u32 { here().line }
                fn col() -> u32 { here().col }
            }
        };
    };
}

macro_rules! Other { derive() ($($t:tt)*) => {}; }

/// A doc comment above the derive.
#[derive(Other,   Placed)]
struct S;
";

fn run_status(main: &str) -> u8 {
    let text = format!("{RUN_HEAD}\n#[unsafe(no_mangle)]\npub extern \"C\" fn main() -> i32 {{ {main} as i32 }}\n");
    let files = [SourceFile { path: ROOT, text: &text }];
    run_with(&files, ROOT, None, FrontChoice::New(NEW), blaze_own::testing::heap().shared())
        .unwrap_or_else(|e| panic!("TRACK CALLER EXPANSIONS: refused: {e}"))
}

/// The built program's `Location` of a call a derive wrote is the derive path's line and
/// column, as rustc gives; the twin reads the column, which the line alone does not test (the
/// path stands after `Other,   `). The control: the doc comment's line (the entry's first byte,
/// the answer before the fix) is one less.
///
/// Prints: `TRACK CALLER EXPANSIONS: derive path at <line>:<col>`
#[test]
fn a_derived_fns_location_is_the_derive_paths_line_and_column() {
    let (line, col) = line_col(RUN_HEAD, "Placed)]");
    let (doc_line, _) = line_col(RUN_HEAD, "/// A doc comment above the derive.");
    assert_eq!(doc_line + 1, line, "the control: the doc comment is the line above the derive");
    assert_eq!(u32::from(run_status("S::line()")), line, "the derive path's line");
    assert_eq!(u32::from(run_status("S::col()")), col, "the derive path's column");
    println!("TRACK CALLER EXPANSIONS: derive path at {line}:{col}");
}

/// Runtime `Location::caller()` check for:
/// 1. an attribute macro inside `#[cfg_attr(..)]` generating a module with a fn calling a
///    `#[track_caller]` fn (`ConfigurableAccountConfigTests` shape), and
/// 2. a nested macro-in-macro generating a module + fn calling a `#[track_caller]` fn.
#[test]
fn a_cfg_attr_and_nested_macro_module_location_points_to_the_call_site() {
    let head = "#![feature(no_core, lang_items, intrinsics, rustc_attrs, macro_attr, macro_derive)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = \"sized\"] trait Sized {}
#[lang = \"copy\"] trait Copy {}
impl Copy for u32 {}

#[lang = \"panic_location\"]
struct Location<'a> { file: &'a str, line: u32, col: u32 }

#[rustc_intrinsic]
const fn caller_location() -> &'static Location<'static>;

#[track_caller]
fn here() -> &'static Location<'static> { caller_location() }

macro_rules! Other { derive() ($($t:tt)*) => {}; }

macro_rules! add_tests {
    attr($($a:tt)*) ($(#[$m:meta])* struct $name:ident;) => {
        struct $name;
        mod attr_tests {
            pub fn line() -> u32 { super::here().line }
            pub fn col() -> u32 { super::here().col }
        }
    };
}

macro_rules! inner_mod_fn {
    () => {
        pub fn line() -> u32 { super::here().line }
        pub fn col() -> u32 { super::here().col }
    };
}

macro_rules! outer_mod {
    () => {
        mod nested_tests {
            inner_mod_fn!();
        }
    };
}

/// Doc comment above cfg_attr struct.
#[derive(Other)]
#[cfg_attr(all(),   add_tests(rlp))]
struct T;

/// Doc comment above nested macro.
    outer_mod!();
";
    let run = |expr: &str| -> u32 {
        let text = format!("{head}\n#[unsafe(no_mangle)]\npub extern \"C\" fn main() -> i32 {{ {expr} as i32 }}\n");
        let files = [SourceFile { path: ROOT, text: &text }];
        u32::from(
            run_with(&files, ROOT, None, FrontChoice::New(NEW), blaze_own::testing::heap().shared())
                .unwrap_or_else(|e| panic!("TRACK CALLER EXPANSIONS ({expr}): refused: {e}")),
        )
    };
    let (attr_line, attr_col) = line_col(head, "add_tests(rlp))]");
    assert_eq!(run("attr_tests::line()"), attr_line, "cfg_attr attribute macro line");
    assert_eq!(run("attr_tests::col()"), attr_col, "cfg_attr attribute macro col");
    let (nested_line, nested_col) = line_col(head, "outer_mod!();");
    assert_eq!(run("nested_tests::line()"), nested_line, "nested macro module line");
    assert_eq!(run("nested_tests::col()"), nested_col, "nested macro module col");
    println!("TRACK CALLER EXPANSIONS: cfg_attr at {attr_line}:{attr_col}, nested macro at {nested_line}:{nested_col}");
}
