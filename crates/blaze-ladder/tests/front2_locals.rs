//! Items declared inside a body, through the real front end with the installed toolchain
//! (`$HOME/.blaze/v2/toolchain`, as tests/front2_checks.rs): each fixture is a fn whose BODY is
//! checked by front2 and must check clean, or be refused with the finding named.
//!
//! The Reference, "Statements", "Item declarations" (fetched 2026-11-16):
//! [statement.item.intro] "An item declaration statement has a syntactic form identical to an
//! item declaration within a module." [statement.item.scope] "Declaring an item within a
//! statement block restricts its scope to the block containing the statement. The item is not
//! given a canonical path nor are any sub-items it may declare."
//! [statement.item.associated-scope] "The exception to this is that associated items defined by
//! implementations are still accessible in outer scopes as long as the item and, if applicable,
//! trait are accessible. It is otherwise identical in meaning to declaring the item inside a
//! module." [statement.item.outer-generics] "There is no implicit capture of the containing
//! function's generic parameters, parameters, and local variables."
//! "Names", "Scopes", item scopes: an item declared in a block is in scope in the whole block,
//! before and after its declaration, and shadows the outer names of its namespace.
//!
//! The pinned core's `assert_unsafe_precondition!` (library/core/src/ub_checks.rs) expands, in
//! the calling body, to a block that declares `const fn precondition_check($($name:$ty),*) {..}`
//! and then calls `precondition_check($($arg,)*)`: `local_macro` is its shape, and
//! `core::ptr::read` and `core::ptr::write_bytes` (which call it) are checked by their paths.
//!
//! Traits imported in a block (front2 block_traits.rs). The Reference, "Use declarations"
//! [items.use.intro]: "These declarations may appear in modules and blocks, usually at the
//! top." [items.use.as-underscore.intro]: "Items can be imported without binding to a name by
//! using an underscore with the form use path as _. This is particularly useful to import a
//! trait so that its methods may be used without importing the trait's symbol". "Method call
//! expressions": "Any of the methods provided by a visible trait implemented by T". Expected
//! lines: `block_use_underscore: clean`, `block_use_named: clean`, `block_use_glob: clean`,
//! `block_use_by_macro: clean`, `block_use_absent: clean (refused with NoMethod)`,
//! `block_use_out_of_block: clean (refused with NoMethod)`.
//!
//! A block's `use` of a name whose type-namespace binding is private (the Reference, "Use
//! declarations", "Visibility and privacy": an import takes in each namespace only a binding it
//! may name; core's `mod poll_fn;` beside `pub use poll_fn::{PollFn, poll_fn};`, imported by
//! tokio's `Receiver::recv` as `use std::future::poll_fn;`). Expected lines:
//! `block_use_private_namesake: clean`, `block_use_private_unexported: clean (stopped with the
//! gap ..)`. Before the change the first stopped with that gap too (21 tempo bodies waited on
//! `recv`'s hidden type for it, c0410wave90).
//!
//! Traits declared in a block (front2 block_traits.rs `Reader::declared`; the block is a module
//! of the names library, blaze-scope blocks.rs). The Reference, "Statements"
//! [statement.item.scope] above, and "Method call expressions": "Then all remaining methods in
//! scope are looked up": a trait declared in a block is in scope in that block. The pinned
//! core's `Write::write_fmt` (library/core/src/fmt/mod.rs) declares `trait SpecWriteFmt` and
//! `impl<W: Write + ?Sized> SpecWriteFmt for &mut W` in its body and calls
//! `self.spec_write_fmt(args)`: `block_trait_write_fmt` is its shape. Expected lines:
//! `block_trait_write_fmt: clean`, `block_trait_inner_call: clean`, `block_trait_provided:
//! clean`, `block_trait_out_of_block: clean (refused with NoMethod)`,
//! `block_trait_path_elsewhere: clean (refused with UnresolvedPath)`. Before the change, the
//! first three print "no head"/"does not lower"/a `Local` gap naming "a trait declared in a
//! block".

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
fn local_fn() -> u8 { fn inner() -> u8 { 1 } inner() }
fn local_fn_before() -> u8 { let x = inner(); fn inner() -> u8 { 1 } x }
fn local_const() -> u8 { const K: u8 = 2; K }
fn local_struct() -> u8 { struct P(u8); impl P { fn get(&self) -> u8 { self.0 } } P(3).get() }
fn local_use() -> u8 { use core::cmp::max; max(1, 2) }
fn local_shadows() -> u8 { fn outer() -> u8 { 5 } outer() }
fn outer() -> bool { true }
fn out_of_block(x: u8) -> u8 { { fn inner() -> u8 { 4 } } inner() }
mod tr {
    pub trait Twice { fn twice(&self) -> u8; }
    impl Twice for u8 { fn twice(&self) -> u8 { *self * 2 } }
}
macro_rules! twice_of {
    ($e:expr) => {{ use $crate::tr::Twice as _; $e.twice() }};
}
fn block_use_underscore(x: &u8) -> u8 { use core::ops::Not as _; x.not() }
fn block_use_named(x: u8) -> u8 { { use crate::tr::Twice; x.twice() } }
fn block_use_glob(x: u8) -> u8 { use crate::tr::*; x.twice() }
fn block_use_by_macro(x: u8) -> u8 { twice_of!(x) }
fn block_use_absent(x: &u8) -> u8 { x.not() }
fn block_use_out_of_block(x: u8) -> u8 { { use crate::tr::Twice as _; } x.twice() }
macro_rules! check_shape {
    ($e:expr) => {
        {
            const fn precondition_check(a: usize) {
                if !(a > 0) {}
            }
            precondition_check($e);
        }
    };
}
fn local_macro(n: usize) { check_shape!(n); }
mod sink {
    pub trait Sink { fn put(&mut self, b: u8) -> u8; }
    impl Sink for u8 { fn put(&mut self, b: u8) -> u8 { *self = b; b } }
}
fn block_trait_write_fmt(w: &mut u8) -> u8 {
    trait SpecPut { fn spec_put(self, b: u8) -> u8; }
    impl<W: crate::sink::Sink + ?Sized> SpecPut for &mut W { fn spec_put(self, b: u8) -> u8 { self.put(b) } }
    w.spec_put(7)
}
fn block_trait_inner_call(w: &mut u8) -> u8 {
    trait SpecPut { fn spec_put(self, b: u8) -> u8; }
    impl<W: crate::sink::Sink + ?Sized> SpecPut for &mut W { fn spec_put(self, b: u8) -> u8 { self.put(b) } }
    { let r = w.spec_put(8); r }
}
fn block_trait_provided(x: u8) -> u8 {
    trait Doubled { fn get(&self) -> u8; fn doubled(&self) -> u8 { self.get() * 2 } }
    impl Doubled for u8 { fn get(&self) -> u8 { *self } }
    x.doubled()
}
fn block_trait_out_of_block(x: u8) -> u8 {
    { trait Tripled { fn tripled(&self) -> u8 { 3 } } impl Tripled for u8 {} }
    x.tripled()
}
fn block_trait_path_elsewhere(w: &mut u8) -> u8 { SpecPut::spec_put(w, 1) }
mod quad {
    pub trait Quad { fn quad(&self) -> u8; }
    impl Quad for u8 { fn quad(&self) -> u8 { *self * 4 } }
    pub mod deep {
        pub fn block_glob_super(x: u8) -> u8 { { use super::*; x.quad() } }
    }
}
mod fut {
    mod poll_fn { pub fn poll_fn(x: u8) -> u8 { x } }
    pub use self::poll_fn::poll_fn;
}
mod fut_unexported {
    mod poll_fn { pub fn poll_fn(x: u8) -> u8 { x } }
}
fn block_use_private_namesake(x: u8) -> u8 { use crate::fut::poll_fn; use crate::tr::Twice as _; poll_fn(x).twice() }
fn block_use_private_unexported(x: u8) -> u8 { use crate::fut_unexported::poll_fn; use crate::tr::Twice as _; x.twice() }
";

/// The bodies that must check clean, each with the rule it proves.
const CLEAN: &[(&str, &str)] = &[
    ("local_fn", "a block's fn is in scope in its block"),
    ("local_fn_before", "a block's fn is in scope before its declaration"),
    ("local_const", "a block's const"),
    ("local_struct", "a block's struct and its inherent impl"),
    ("local_use", "a block's use declaration"),
    ("local_shadows", "a block's fn shadows the module's of the same name"),
    ("local_macro", "core's `assert_unsafe_precondition!` shape: an expansion's block declares a const fn and calls it"),
    ("block_use_underscore", "a block's `use Trait as _;` puts the trait's methods in scope there"),
    ("block_use_named", "an inner block's named `use` of a trait puts its methods in scope there"),
    ("block_use_glob", "a block's glob `use` of a module puts the module's traits in scope there"),
    ("block_use_by_macro", "tracing's shape: an expansion's block writes `use $crate::..::Trait as _;` and calls its method"),
    ("block_trait_write_fmt", "core's `Write::write_fmt` shape: a block declares a trait and its impl for `&mut W` and calls the method"),
    ("block_trait_inner_call", "a block's trait is in scope in the blocks inside it"),
    ("block_trait_provided", "a block's trait with a provided method, called through the block's impl"),
    (
        "block_use_private_namesake",
        "tokio's `Receiver::recv` shape (`use std::future::poll_fn;` over core's private `mod poll_fn` beside `pub use poll_fn::poll_fn`): a block's `use` takes the visible fn, the private module of the name brings no trait",
    ),
];

/// The bodies whose check must stop with a gap naming the text (front2 block_traits.rs).
/// `block_use_private_unexported` is `block_use_private_namesake` without the `pub use` (the
/// cause removed): the `use` names only a private module (rustc: E0603), no namespace resolves
/// it, and the block's traits are refused by name, never read as "none".
const REFUSED_GAP: &[(&str, &str)] = &[("block_use_private_unexported", blaze_ladder::front2::block_traits::UNRESOLVED)];

/// The bodies that must be refused, with the finding each must have. `block_use_absent` is
/// `block_use_underscore` without its `use` (the cause removed: `Not` is in no prelude);
/// `block_use_out_of_block` imports in an inner block and calls outside it (the Reference,
/// "Use declarations": a block's import is in scope in that block only).
/// `block_trait_out_of_block` declares a trait (and its impl) in an inner block and calls its
/// method outside it, and `block_trait_path_elsewhere` names another fn's block trait by path
/// (the Reference, "Statements": "Declaring an item within a statement block restricts its
/// scope to the block containing the statement").
const REFUSED: &[(&str, &str)] = &[
    ("out_of_block", "UnresolvedPath"),
    ("block_use_absent", "NoMethod"),
    ("block_use_out_of_block", "NoMethod"),
    ("block_trait_out_of_block", "NoMethod"),
    ("block_trait_path_elsewhere", "UnresolvedPath"),
];

/// Bodies below the root that must check clean, by path from the root. `block_glob_super`: a
/// block's `use super::*;` names `quad`, the parent of the body's module (front2
/// block_traits.rs `Reader::glob` through `names::written_module`), so `Quad`'s method is in
/// scope there.
const CLEAN_AT: &[(&str, &[&str], &str)] = &[(
    "block_glob_super",
    &["quad", "deep", "block_glob_super"],
    "a block's `use super::*;` brings the parent module's traits",
)];

/// Library fns that use `assert_unsafe_precondition!`, by path from their crate's root.
const LIBRARY: &[(&str, &[&str])] = &[("core", &["ptr", "read"]), ("core", &["ptr", "write_bytes"])];

#[test]
fn items_inside_bodies_through_front2() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/locs/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "locs", dir: "/locs/", root: "/locs/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
        let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let find = |krate: usize, segments: &[&str]| {
        names::path(&names, names.root(krate), Namespace::Values, Written { kind: PathKind::Plain, segments }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(segments.len()))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{segments:?}` from crate {krate}'s root"))
    };
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut failed = Vec::new();
    let mut report: Vec<String> = Vec::new();
    let mut cases: Vec<(String, blaze_id::ItemKey, &str, Option<&str>)> = Vec::new();
    for &(n, p) in CLEAN {
        cases.push((n.to_string(), find(0, &[n]), p, None));
    }
    for &(n, w) in REFUSED {
        cases.push((n.to_string(), find(0, &[n]), "must be refused", Some(w)));
    }
    for &(n, w) in REFUSED_GAP {
        cases.push((n.to_string(), find(0, &[n]), "must stop with a gap", Some(w)));
    }
    for &(n, path, p) in CLEAN_AT {
        cases.push((n.to_string(), find(0, path), p, None));
    }
    for &(krate, path) in LIBRARY {
        let ix = crates.iter().position(|c| c.name == krate).unwrap_or_else(|| panic!("{krate} among the crates"));
        cases.push((format!("{krate}::{}", path.join("::")), find(ix, path), "a library fn calling `assert_unsafe_precondition!`", None));
    }
    for (name, key, proves, want) in cases {
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Fn(key)) {
            report.push(format!("{name}: no head: {e:?}"));
            failed.push(name);
            continue;
        }
        let body = match lower_fn(&names, prepared, &mut front.keys, &mut store, key) {
            Ok(b) => b,
            Err(e) => {
                report.push(format!("{name}: does not lower: {e}"));
                failed.push(name);
                continue;
            }
        };
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        match (check_body(&mut records, &body, &scope), want) {
            (Ok(_), None) => report.push(format!("{name}: clean")),
            (Err(CheckFail::Diags { diags, .. }), Some(w)) if format!("{diags:?}").contains(&format!("({w},")) => {
                report.push(format!("{name}: clean (refused with {w})"));
            }
            (Err(gap @ CheckFail::Gap { .. }), Some(w)) if format!("{gap:?}").contains(w) => {
                report.push(format!("{name}: clean (stopped with the gap `{w}`)"));
            }
            (Ok(_), Some(w)) => {
                report.push(format!("{name}: checked clean, but must be refused with {w}"));
                failed.push(name);
            }
            (Err(CheckFail::Diags { diags, .. }), _) => {
                report.push(format!("{name}: diagnostics {diags:?} ({proves})"));
                failed.push(name);
            }
            (Err(other), _) => {
                report.push(format!("{name}: {other:?} ({proves})"));
                failed.push(name);
            }
        }
    }
    // Failures first: the harness's runner shows the first lines only.
    report.sort_by_key(|l| l.contains(": clean"));
    for line in &report {
        println!("{line}");
    }
    let why: Vec<String> = report.iter().filter(|l| !l.contains(": clean")).map(|l| l.chars().take(300).collect()).collect();
    assert!(failed.is_empty(), "bodies that did not check as they must: {failed:?}\n{}", why.join("\n"));
}
