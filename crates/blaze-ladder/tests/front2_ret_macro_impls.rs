//! `impl Trait` in a fn's return type that a TYPE MACRO writes, at any depth, and one nested in
//! an `async fn`'s written return type: each is an opaque type of its fn.
//!
//! The rule. The Reference, "Impl trait", "Abstract return types": "Functions can use `impl`
//! followed by a set of trait bounds to declare a return type as being abstract"; "the caller can
//! only use the functionality specified by the trait bounds". A type macro is read as the type it
//! expands to (the Reference, "Macros By Example": a macro invocation in type position is
//! "transcribed" from the matched rule and "the macro expander ... replaces the invocation" with
//! it), so `fn made() -> it!(u8)` with `macro_rules! it { ($t:ty) => { impl Iterator<Item = $t> }
//! }` is `fn made() -> impl Iterator<Item = u8>`. "Async functions": "async fn example(x: &str) ->
//! usize" is "roughly equivalent to" "fn example<'a>(x: &'a str) -> impl Future<Output = usize> +
//! 'a"; an `impl Trait` inside the written `usize` (`Result<impl Clone, ()>`) is one more opaque of
//! the fn. The library form this stands for is alloy-transport's `impl_future!(<Output = T>)`
//! (`impl Future<Output = T> + Send + 'static`), which tempo's `TempoProviderExt` methods
//! return, and `NetworkWallet::sign_request`, `TransportConnect::get_transport` and
//! `PubSubConnect::connect` too.
//!
//! What a wrong fix fails. Before front2/type_expand.rs, `fn_head` collected the return opaques
//! from the UNEXPANDED signature and `macro_ty` read the expansion's `impl` against ids of the
//! item's view: `made`, `in_option`, `fut_af`, `<trait Mk>::mk` had no head ("an `impl Trait` type
//! outside a fn's parameters and return type"), and every caller of them failed with it. A fix
//! that files the expansion's opaque with no bounds makes `use_made` (`i.next()` through the
//! `Iterator` bound) and `use_fut` (`.await` through `Future<Output = u16>`) fail, and checks
//! `lacks` clean (its hidden `u8` is no `Iterator`: E0277). A fix that numbers or keys the
//! expansion's opaque against the item's view mismatches `in_option`'s `Option<..>`.
//!
//! The run prints, and must print, exactly: `return-type macros: 9 clean, 1 refused, 0 failed`.

#![feature(variant_count)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::{MemberKind, checker_of};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::resolve::Resolver;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
struct Count(u8);
impl Iterator for Count { type Item = u8; fn next(&mut self) -> Option<u8> { None } }
macro_rules! it { ($t:ty) => { impl Iterator<Item = $t> } }
macro_rules! fut { ($t:ty) => { impl core::future::Future<Output = $t> } }
fn made() -> it!(u8) { Count(0) }
fn use_made() -> Option<u8> { let mut i = made(); i.next() }
fn in_option() -> Option<it!(u8)> { None::<Count> }
fn use_in_option() -> Option<u8> { match in_option() { Some(mut i) => i.next(), None => None } }
trait Mk { fn mk(&self) -> it!(u8); }
impl Mk for u8 { fn mk(&self) -> it!(u8) { Count(*self) } }
fn use_mk<M: Mk>(m: &M) -> Option<u8> { let mut i = m.mk(); i.next() }
async fn af(x: u8) -> u16 { x as u16 }
fn fut_af() -> fut!(u16) { af(1) }
async fn use_fut() -> u16 { fut_af().await }
async fn nested_async(x: u8) -> Result<impl Clone, ()> { Ok(x) }
async fn use_nested_async() -> bool { nested_async(1).await.is_ok() }
fn lacks() -> it!(u8) { 1u8 }
";

/// The bodies that must check clean, each with what it proves.
const CLEAN: [(&str, &str); 9] = [
    ("made", "`-> it!(u8)`: the expansion's `impl Iterator<Item = u8>` is opaque 0, hidden type `Count`"),
    ("use_made", "`i.next()` through the expansion's `Iterator<Item = u8>` bound gives `Option<u8>`"),
    ("in_option", "`-> Option<it!(u8)>`: a macro nested in the return type; the opaque inside `Option` is `Count`"),
    ("use_in_option", "a method through the nested expansion's bound, bound by a pattern inside `Option`"),
    ("use_mk", "a trait's method `-> it!(u8)` (return-position `impl Trait` in a trait) has its opaque's bound"),
    ("fut_af", "`-> fut!(u16)`, as alloy's `impl_future!`: the hidden type is `af`'s future"),
    ("use_fut", "`.await` on the expansion's `Future<Output = u16>` is `u16`"),
    ("nested_async", "`async fn .. -> Result<impl Clone, ()>`: the written `impl` is opaque 1 after the future's 0"),
    ("use_nested_async", "`nested_async(1).await` is `Result<opaque 1, ()>`; `.is_ok()` is `bool`"),
];

/// The bodies that must be refused, each with the diagnostic it must give.
const REFUSED: [(&str, &str, &str); 1] = [("lacks", "Unsatisfied", "the hidden type `u8` is not `Iterator` (the expansion's bound): E0277")];

#[test]
fn return_type_macro_impls_through_front2() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/rmac/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "rmac", dir: "/rmac/", root: "/rmac/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let root = names.root(0);
    let lib = |name: &'static str| {
        names::path(&names, root, Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"))
    };
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut failed: Vec<String> = Vec::new();
    let mut clean = 0usize;
    let mut refused = 0usize;
    for (name, proves) in CLEAN {
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Fn(lib(name))) {
            failed.push(format!("{name}: no head: {e:?} ({proves})"));
            continue;
        }
        let body = match lower_fn(&names, prepared, &mut front.keys, &mut store, lib(name)) {
            Ok(b) => b,
            Err(e) => {
                failed.push(format!("{name}: does not lower: {e} ({proves})"));
                continue;
            }
        };
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        match check_body(&mut records, &body, &scope) {
            Ok(_) => clean += 1,
            Err(CheckFail::Diags { diags, .. }) => failed.push(format!("{name}: diagnostics {diags:?} ({proves})")),
            Err(other) => failed.push(format!("{name}: {other:?} ({proves})")),
        }
    }
    for (name, code, proves) in REFUSED {
        let body = match store.ensure(&names, &mut front.keys, Want::Fn(lib(name))).map_err(|e| format!("{e:?}")).and_then(|_| {
            lower_fn(&names, prepared, &mut front.keys, &mut store, lib(name)).map_err(|e| format!("{e}"))
        }) {
            Ok(b) => b,
            Err(e) => {
                failed.push(format!("{name}: no body to refuse: {e} ({proves})"));
                continue;
            }
        };
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        match check_body(&mut records, &body, &scope) {
            Err(CheckFail::Diags { diags, .. }) if format!("{diags:?}").contains(code) => refused += 1,
            Ok(_) => failed.push(format!("{name}: checked clean, not refused with {code} ({proves})")),
            Err(other) => failed.push(format!("{name}: not refused with {code} ({proves}): {other:?}")),
        }
    }
    // `impl Mk for u8`'s `fn mk(&self) -> it!(u8)` has its signature: the member head is built
    // from the expanded return type, as the trait's is.
    let resolver = Resolver { names: &names };
    for (k, _, _) in names.items(0) {
        if !matches!(resolver.detail(k), Some(blaze_surface::ItemDetail::Impl)) {
            continue;
        }
        let header = names.item_text(k).and_then(|t| t.impl_header).unwrap_or("?");
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Impl(k)) {
            failed.push(format!("impl `{header}`: no head: {e}"));
            continue;
        }
        for m in names.members(k).unwrap_or(&[]) {
            let Some(mk) = m.name.and_then(|n| front.keys.filed_member(checker_of(k), MemberKind::Fn, n)) else { continue };
            if let Some(why) = store.fn_gaps.get(&mk) {
                failed.push(format!("impl `{header}`: member `{}` has no signature: {why:?}", m.name.unwrap_or("?")));
            }
        }
    }
    for f in &failed {
        println!("{f}");
    }
    println!("return-type macros: {clean} clean, {refused} refused, {} failed", failed.len());
    assert!(failed.is_empty(), "{} of the return-type macro cases failed (lines above)", failed.len());
    assert_eq!((clean, refused), (CLEAN.len(), REFUSED.len()));
}
