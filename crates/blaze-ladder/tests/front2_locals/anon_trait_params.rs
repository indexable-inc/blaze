//! Rust 2015 anonymous trait method parameters (RFC 1685, e.g. `fxhash 0.2.1`'s
//! `trait HashWord { fn hash_word(&mut self, Self); }`), verified end-to-end through
//! `Front2Program`, `LazyProgram`, `Store` heads, `fn_life` + `regions` (lifetime elision),
//! dyn compatibility, and native execution via `run_with`.
//!
//! Negative controls (Rule 23):
//! - Rust 2018, 2021, and 2024 reject anonymous trait method parameters.
//! - Even in Rust 2015, free `fn`s, `impl` methods, and trait methods with default bodies
//!   reject anonymous parameters.

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::{MemberKind, checker_of, member_key_of};
use blaze_ladder::front2::lifetimes::Region;
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::front2::program::lazy::LazyProgram;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};
use blaze_ladder::seam::FrontEnd;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

use super::PRELUDE;

/// Comprehensive Rust 2015 program exercising `fxhash 0.2.1`'s `HashWord`, complex anonymous
/// parameter types (`Self`, paths, `&'a T`, `&mut [u8]`, `*const u8`, `*mut u8`, tuples, arrays,
/// bare 2015 trait objects, `fn(...) -> ...`, `MyOption<fn(u8) -> bool>`, `impl Trait`,
/// `#[allow(unused)] u8`, `self: &Self`), lifetime elision (`fn(&u8) -> &u8`,
/// `fn(&self, &u8) -> &u8`), default trait methods (calling anonymous-param methods and using
/// tuple / `ref` patterns in 2015), and dyn dispatch on an anonymous-param trait.
const RUST_2015_PROGRAM: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs, arbitrary_self_types)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

enum MyOption<T> {
    None,
    Some(T),
}

trait Marker {
    fn tag(&self) -> u8;
}

struct Tag(u8);

impl Marker for Tag {
    fn tag(&self) -> u8 {
        self.0
    }
}

trait HashWord {
    fn hash_word(&mut self, Self);

    fn hash_twice(&mut self, x: Self)
    where
        Self: Copy,
    {
        self.hash_word(x);
        self.hash_word(x);
    }

    fn sum_tuple(&self, (a, b): (u8, u8)) -> u8 {
        a + b
    }

    fn read_ref(&self, ref x: u8) -> u8 {
        *x
    }

    fn bump_mut(&self, mut x: u8) -> u8 {
        x = x + 0;
        x
    }
}

impl HashWord for u8 {
    fn hash_word(&mut self, word: u8) {
        *self = *self + word;
    }
}

trait ComplexAnon {
    fn more(
        &self,
        u8,
        &[u8],
        &mut [u8],
        *const u8,
        *mut u8,
        (u8, u8),
        [u8; 2],
        &Marker,
        MyOption<fn(u8) -> bool>,
        #[allow(unused)] u8,
        named: u8,
        _: u8,
    ) -> u8;

    fn typed_self(self: &Self, u8) -> u8;
    fn impl_param(&self, impl Copy) -> u8;
}

struct Acc(u8);

impl ComplexAnon for Acc {
    fn more(
        &self,
        a: u8,
        s: &[u8],
        ms: &mut [u8],
        _p: *const u8,
        _mp: *mut u8,
        t: (u8, u8),
        arr: [u8; 2],
        m: &Marker,
        f: MyOption<fn(u8) -> bool>,
        attr_arg: u8,
        named: u8,
        _ignored: u8,
    ) -> u8 {
        let bonus = match f {
            MyOption::Some(cb) => if cb(a) { 1 } else { 0 },
            MyOption::None => 0,
        };
        ms[0] = ms[0] + s[0];
        self.0 + a + ms[0] + t.0 + t.1 + arr[0] + arr[1] + m.tag() + bonus + attr_arg + named
    }

    fn typed_self(self: &Self, x: u8) -> u8 {
        self.0 + x
    }

    fn impl_param(&self, _x: impl Copy) -> u8 {
        self.0
    }
}

trait Elide {
    fn echo(&u8) -> &u8;
    fn with_self(&self, &u8) -> &u8;
    fn explicit<'a>(&'a u8, &'a u8) -> &'a u8;
}

struct Holder {
    v: u8,
}

impl Elide for Holder {
    fn echo(x: &u8) -> &u8 {
        x
    }

    fn with_self(&self, _other: &u8) -> &u8 {
        &self.v
    }

    fn explicit<'a>(a: &'a u8, _b: &'a u8) -> &'a u8 {
        a
    }
}

trait DynAnon {
    fn add(&self, u8) -> u8;
}

impl DynAnon for Acc {
    fn add(&self, n: u8) -> u8 {
        self.0 + n
    }
}

fn is_nonzero(x: u8) -> bool {
    x != 0
}

fn call_dyn(d: &DynAnon, n: u8) -> u8 {
    d.add(n)
}

#[no_mangle]
pub extern \"C\" fn main() -> i32 {
    let mut h: u8 = 1;
    h.hash_word(2);
    h.hash_twice(3); // h = 1 + 2 + 3 + 3 = 9
    let t = h.sum_tuple((4, 5)); // 9
    let r = h.read_ref(2); // 2
    let m = h.bump_mut(0); // 0

    let acc = Acc(1);
    let ro: [u8; 2] = [2, 0];
    let mut rw: [u8; 2] = [3, 0];
    let tag = Tag(4);
    let p: *const u8 = &ro[0];
    let mp: *mut u8 = &mut rw[1];
    // more: 1 + 2 + (3+2) + 1 + 2 + 1 + 1 + 4 + 1 + 2 + 3 = 23
    let c = acc.more(
        2,
        &ro,
        &mut rw,
        p,
        mp,
        (1, 2),
        [1, 1],
        &tag,
        MyOption::Some(is_nonzero),
        2,
        3,
        99,
    );
    let ts = acc.typed_self(5); // 6
    let ip = acc.impl_param(7u8); // 1

    let holder = Holder { v: 11 };
    let x = 6u8;
    let y = 7u8;
    let e1 = *Holder::echo(&x); // 6
    let e2 = *holder.with_self(&y); // 11
    let e3 = *Holder::explicit(&x, &y); // 6

    let d = call_dyn(&acc, 4); // 5

    // 9 + 9 + 2 + 0 + 23 + 6 + 1 + 6 + 11 + 6 + 5 = 78
    (h + t + r + m + c + ts + ip + e1 + e2 + e3 + d) as i32
}
";

const MINIMAL_ANON_TRAIT: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs)]
#![no_core]
#![allow(internal_features)]

#[lang = \"sized\"]
trait Sized {}

trait HashWord {
    fn hash_word(&mut self, Self);
}

impl HashWord for u8 {
    fn hash_word(&mut self, x: u8) {
        *self = x;
    }
}

fn main() {
    let mut x: u8 = 0;
    x.hash_word(1);
}
";

const FREE_FN_ANON: &str = "#![feature(no_core, lang_items)]
#![no_core]

#[lang = \"sized\"]
trait Sized {}

fn bad(u32) {}

fn main() {
    bad(1);
}
";

const IMPL_METHOD_ANON: &str = "#![feature(no_core, lang_items)]
#![no_core]

#[lang = \"sized\"]
trait Sized {}

struct S;

impl S {
    fn bad(&self, u32) {}
}

fn main() {
    let s = S;
    s.bad(1);
}
";

const TRAIT_DEFAULT_BODY_ANON: &str = "#![feature(no_core, lang_items)]
#![no_core]

#[lang = \"sized\"]
trait Sized {}

trait T {
    fn bad(&self, u32) {}
}

struct S;
impl T for S {}

fn main() {
    let s = S;
    s.bad(1);
}
";

fn with_crate(
    edition: &'static str,
    text: &'static str,
    with_prelude: bool,
    f: impl FnOnce(&blaze_scope::resolve::Names<'_>, &[SourceFile<'_>]),
) {
    let files_one = [SourceFile { path: "src/main.rs", text }];
    let files_two = [
        SourceFile { path: "src/main.rs", text },
        SourceFile { path: "src/prelude.rs", text: PRELUDE },
    ];
    let files: &[SourceFile<'_>] = if with_prelude { &files_two } else { &files_one };
    let tc = Toolchain::none(blaze_own::testing::heap().alloc());
    let new = NewFront { package: "anon_trait", edition, shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = blaze_ladder::program_texts::ProgramTexts::new(files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind)
        .unwrap_or_else(|e| panic!("ProgramTexts({edition}): {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("Inputs({edition}): {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve({edition}): {e}"));
    f(&names, files);
}

fn lib_key_of(names: &blaze_scope::resolve::Names<'_>, ns: Namespace, name: &'static str) -> blaze_id::ItemKey {
    let segments: &[&str] = &[name];
    let written = Written { kind: PathKind::Plain, segments };
    let found = names::path(names, names.root(0), ns, written, blaze_own::testing::heap().alloc())
        .unwrap_or_else(|e| panic!("`{name}`: {e:?}"));
    found.whole(1).unwrap_or_else(|| panic!("`{name}` resolves whole")).key
}

/// Rust 2015 builds, type-checks, borrow-checks, and executes `HashWord` (`fxhash 0.2.1` shape),
/// complex anonymous parameter types, lifetime elision, default trait methods, and dyn dispatch.
#[test]
fn rust_2015_anonymous_trait_params_run_end_to_end() {
    let new = NewFront { package: "anon_2015", edition: "2015", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let files = [
        SourceFile { path: "src/main.rs", text: RUST_2015_PROGRAM },
        SourceFile { path: "src/prelude.rs", text: PRELUDE },
    ];
    let got = run_with(&files, "src/main.rs", None, FrontChoice::New(new), blaze_own::testing::heap().shared())
        .unwrap_or_else(|e| panic!("Rust 2015 anonymous trait params run failed: {e}"));
    assert_eq!(got, 78, "Rust 2015 anonymous trait params native execution");
}

/// Rust 2015 `Front2Program`, `LazyProgram`, `Store` heads, `fn_life`, head regions, and dyn
/// compatibility all handle anonymous trait method parameters cleanly.
#[test]
fn rust_2015_heads_lifetimes_dyn_and_lazy_program_succeed() {
    with_crate("2015", RUST_2015_PROGRAM, true, |names, files| {
        let alloc = blaze_own::testing::heap().alloc();
        let (wf, lf, hf) = (
            blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame"),
            blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame"),
            blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame"),
        );
        let whole = Front2Program::new(names, &wf, files, alloc)
            .unwrap_or_else(|e| panic!("Front2Program (2015): {e}"));
        let main = checker_of(lib_key_of(names, Namespace::Values, "main"));
        let mut lazy = LazyProgram::new(names, &lf, files, Some(main), alloc)
            .unwrap_or_else(|e| panic!("LazyProgram (2015): {e}"));
        lazy.check_one(main).unwrap_or_else(|e| panic!("check_one(main): {e}")).expect("main has a body");

        // `DynAnon` (anonymous `u8`) is dyn-compatible; `HashWord` (anonymous `Self`) is not.
        let dyn_anon = checker_of(lib_key_of(names, Namespace::Types, "DynAnon"));
        let hash_word_lib = lib_key_of(names, Namespace::Types, "HashWord");
        let hash_word = checker_of(hash_word_lib);
        assert!(whole.object_trait(dyn_anon).is_some(), "DynAnon is dyn-compatible");
        assert!(lazy.object_trait(dyn_anon).is_some(), "lazy DynAnon is dyn-compatible");
        assert!(whole.object_trait(hash_word).is_none(), "HashWord takes `Self` outside receiver");

        // Check `Store` heads, `fn_life`, and head regions on `HashWord::hash_word` and `Elide`.
        let mut front = Front2::new(names, &hf, files, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
        let mut store = Store::new(front.index.lang("sized"), front.frame, alloc);
        let elide_lib = lib_key_of(names, Namespace::Types, "Elide");
        let elide = checker_of(elide_lib);
        store.ensure(names, &mut front.keys, Want::Members(hash_word_lib)).unwrap_or_else(|e| panic!("HashWord members: {e:?}"));
        store.ensure(names, &mut front.keys, Want::Members(elide_lib)).unwrap_or_else(|e| panic!("Elide members: {e:?}"));

        let hw_fn = member_key_of(hash_word, MemberKind::Fn, "hash_word");
        let echo_fn = member_key_of(elide, MemberKind::Fn, "echo");
        let with_self_fn = member_key_of(elide, MemberKind::Fn, "with_self");
        let explicit_fn = member_key_of(elide, MemberKind::Fn, "explicit");

        for (label, k) in [
            ("HashWord::hash_word", hw_fn),
            ("Elide::echo", echo_fn),
            ("Elide::with_self", with_self_fn),
            ("Elide::explicit", explicit_fn),
        ] {
            assert!(store.fns.contains_key(&k), "{label} head missing; gap={:?}", store.fn_gaps.get(&k));
            assert!(!store.fn_gaps.contains_key(&k), "{label} unexpected gap: {:?}", store.fn_gaps.get(&k));
            assert!(store.regions.made(k).is_some(), "{label} regions unread: {:?}", store.regions.unread(k));
        }

        // Lifetime elision checks via `fn_life`:
        // `fn echo(&u8) -> &u8`: input 0 and output share `Anon(0)`.
        let echo_life = store.fn_life(names, &front.keys, echo_fn).unwrap_or_else(|e| panic!("echo fn_life: {e:?}"));
        assert_eq!(echo_life.regions.as_slice(), &[Region::Anon(0), Region::Anon(0)]);
        // `fn with_self(&self, &u8) -> &u8`: receiver `Anon(0)`, arg 1 `Anon(1)`, output `Anon(0)`.
        let ws_life = store.fn_life(names, &front.keys, with_self_fn).unwrap_or_else(|e| panic!("with_self fn_life: {e:?}"));
        assert_eq!(ws_life.regions.as_slice(), &[Region::Anon(0), Region::Anon(1), Region::Anon(0)]);
        // `fn hash_word(&mut self, Self)`: receiver `Anon(0)` only.
        let hw_life = store.fn_life(names, &front.keys, hw_fn).unwrap_or_else(|e| panic!("hash_word fn_life: {e:?}"));
        assert_eq!(hw_life.regions.as_slice(), &[Region::Anon(0)]);
    });
}

/// Negative control: Rust 2018, 2021, and 2024 MUST reject anonymous trait method parameters.
#[test]
fn rust_2018_2021_2024_reject_anonymous_trait_params() {
    let alloc = blaze_own::testing::heap().alloc();
    for edition in ["2018", "2021", "2024"] {
        with_crate(edition, MINIMAL_ANON_TRAIT, false, |names, files| {
            let (wf, hf) = (
                blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame"),
                blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame"),
            );
            assert!(
                Front2Program::new(names, &wf, files, alloc).is_err(),
                "edition {edition}: anonymous trait parameter must be rejected by Front2Program",
            );

            let mut front = Front2::new(names, &hf, files, alloc).unwrap_or_else(|e| panic!("front2 ({edition}): {e}"));
            let mut store = Store::new(front.index.lang("sized"), front.frame, alloc);
            let hash_word_lib = lib_key_of(names, Namespace::Types, "HashWord");
            let hw_fn = member_key_of(checker_of(hash_word_lib), MemberKind::Fn, "hash_word");
            let _ = store.ensure(names, &mut front.keys, Want::Members(hash_word_lib));
            assert!(
                store.fn_gaps.contains_key(&hw_fn) && !store.fns.contains_key(&hw_fn),
                "edition {edition}: HashWord::hash_word must be refused in fn_gaps",
            );
        });
    }
}

/// Negative control: even in Rust 2015, free `fn`s, `impl` methods, and trait methods with a
/// default body MUST reject anonymous parameters.
#[test]
fn rust_2015_rejects_anonymous_params_in_free_fn_impl_and_bodied_trait_method() {
    let alloc = blaze_own::testing::heap().alloc();
    for (label, text) in [
        ("free fn", FREE_FN_ANON),
        ("impl method", IMPL_METHOD_ANON),
        ("trait method with body", TRAIT_DEFAULT_BODY_ANON),
    ] {
        with_crate("2015", text, false, |names, files| {
            let wf = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
            assert!(
                Front2Program::new(names, &wf, files, alloc).is_err(),
                "Rust 2015 {label}: anonymous parameter must be rejected",
            );
        });
    }
}

const BODILESS_REF_PAT: &str = "#![feature(no_core, lang_items)]
#![no_core]
#[lang = \"sized\"]
trait Sized {}
trait T { fn f(&self, ref x: u8); }
struct S;
impl T for S { fn f(&self, _: u8) {} }
fn main() { S.f(1); }
";

const BODILESS_TUPLE_PAT: &str = "#![feature(no_core, lang_items)]
#![no_core]
#[lang = \"sized\"]
trait Sized {}
trait T { fn f(&self, (a, b): (u8, u8)); }
struct S;
impl T for S { fn f(&self, _: (u8, u8)) {} }
fn main() { S.f((1, 2)); }
";

const BODILESS_MUT_PAT: &str = "#![feature(no_core, lang_items)]
#![no_core]
#[lang = \"sized\"]
trait Sized {}
trait T { fn f(&self, mut x: u8); }
struct S;
impl T for S { fn f(&self, _: u8) {} }
fn main() { S.f(1); }
";

const BODILESS_MUT_SELF: &str = "#![feature(no_core, lang_items)]
#![no_core]
#[lang = \"sized\"]
trait Sized {}
trait T { fn f(mut self); }
struct S;
impl T for S { fn f(self) {} }
fn main() { S.f(); }
";

/// Negative control (E0642): bodiless trait methods reject nontrivial patterns (`ref x`,
/// `(a, b)`, `mut x`, `mut self`) in both Rust 2015 and Rust 2021, while bodied trait methods
/// (`sum_tuple`, `read_ref`, `bump_mut` in `RUST_2015_PROGRAM`) accept them.
#[test]
fn bodiless_trait_methods_reject_patterns_e0642() {
    let alloc = blaze_own::testing::heap().alloc();
    for edition in ["2015", "2021"] {
        for (label, text) in [
            ("ref pat", BODILESS_REF_PAT),
            ("tuple pat", BODILESS_TUPLE_PAT),
            ("mut pat", BODILESS_MUT_PAT),
            ("mut self", BODILESS_MUT_SELF),
        ] {
            with_crate(edition, text, false, |names, files| {
                let wf = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
                assert!(
                    Front2Program::new(names, &wf, files, alloc).is_err(),
                    "edition {edition} bodiless trait method ({label}): pattern must be rejected (E0642)",
                );
            });
        }
    }
}
