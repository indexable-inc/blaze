//! Bounds and types the checker refused that Rust accepts, each reduced from a body left out of
//! tempo's build (census g044908), checked through front2 with the real core; each class with a
//! control the language refuses, which must stay refused. Needs the installed toolchain under
//! `$HOME/.blaze/v2/toolchain`, as tests/front2_bounds.rs does.
//!
//! The classes:
//! - An item a specializing impl does not define is the one of the impl it specializes (RFC
//!   1210, "Specialization"). The pinned core: `unsafe impl StepByBackImpl<Range<u16>> for
//!   StepBy<Range<u16>>` writes no `type Item` beside the blanket `StepByBackImpl` impl's
//!   (core/src/iter/adapters/step_by.rs), so `F: FnMut(Acc, Self::Item) -> Acc` is `F:
//!   FnMut(Acc, u16) -> Acc` (census: `LEFT 8 .. Param(1): FnMut<(Param(0), u16)>` and its
//!   `u8`, `usize`, `u32`, `u64` rows); `impl<I> FuseImpl<I> for Fuse<I> where I:
//!   FusedIterator` none beside `impl<I: Iterator> FuseImpl<I> for Fuse<I>` (fuse.rs; census
//!   `LEFT 28 .. E0282 found Option<Alias ..`). blaze-solve alias.rs `step_impl`.
//! - Generic arguments written on a trait's segment of a path to its item (`Tr::<A>::f`) are
//!   the trait's (the Reference, "Paths", "Paths in expressions"). alloy-sol-types:
//!   `crate::private::SolTypeValue::<Self>::stv_to_tokens(rust)` (census: `Address:
//!   SolTypeValue<Error>`, `CLASS callee 94`). blaze-typeck rules/lit_path.rs `parent_args`.
//! - A method call's expected type reaches its arguments (the Reference, "Type coercions",
//!   "Coercion sites", function arguments; rustc's `expected_inputs_for_expected_output`), so a
//!   fn item argument coerces to the fn pointer the expected type makes the parameter
//!   ("Coercion types": a fn item to a fn pointer). mime_guess 2.0.5 `MimeGuess::iter`: `Iter(self
//!   .iter_raw().map(expect_mime))` (census `found Map<IterRaw, FnDef(..)>`). rules/method.rs.
//! - Autoderef through a user `Deref` impl whose where-clause is on a closure not settled yet
//!   (the Reference, "Field access expressions": "automatically dereferenced as many times as
//!   necessary"). hashbrown 0.14's `ScopeGuard<T, F> where F: FnMut(&mut T)`: `let mut self_ =
//!   guard(self, |self_| self_.clear_no_drop()); self_.table` (census `found ScopeGuard<..>,
//!   about table`). blaze-typeck machine.rs `overloaded_deref`.
//!
//! - A constant of a reference type is no non-reference pattern (the Reference, "Binding
//!   modes"), so it does not peel the scrutinee's reference. jsonwebtoken's `Enc::deserialize`:
//!   `match s.as_str() { ENC_A128CBC_HS256 => .. }` with `const ENC_A128CBC_HS256: &str`
//!   (census `found &str, expected str`, 7). rules/pat.rs `step_pat`.
//!
//! - A fn a module-level `extern` block declares has its block's ABI and is `unsafe` unless
//!   written `safe` (the Reference, "External blocks"). aws-lc-rs `AeadCtx::chacha20` passes
//!   `EVP_aead_chacha20_poly1305` as an `unsafe extern "C" fn` (census `found FnDef, expected
//!   FnPtr(.. abi: C, safety: Unsafe ..)`, 10); libc's `open(path, oflag, ...)` (census `...` on
//!   a fn of an ABI other than "C", 7). front2/heads.rs `fn_head` through foreign_head.rs.
//!
//! What it prints when right: one line, `as stated: [..]` naming every case.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::{lower_fn, lower_member};
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::{MemberKind, checker_of};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = r#"fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
struct Count(u8);
impl Iterator for Count { type Item = u8; fn next(&mut self) -> Option<u8> { None } }
trait Spec<I> {
    type Item;
    fn fold_it<A, F>(self, a: A, f: F) -> A where F: FnMut(A, Self::Item) -> A;
}
struct W<I>(I);
impl<I: Iterator> Spec<I> for W<I> {
    type Item = I::Item;
    default fn fold_it<A, F>(self, a: A, f: F) -> A where F: FnMut(A, Self::Item) -> A { a }
}
impl Spec<Count> for W<Count> {
    fn fold_it<A, F>(self, a: A, mut f: F) -> A where F: FnMut(A, Self::Item) -> A { f(a, 1u8) }
}
fn spec_where<A, F>(a: A, mut f: F) -> A where F: FnMut(A, <W<Count> as Spec<Count>>::Item) -> A { f(a, 1u8) }
fn spec_bad<A, F>(a: A, mut f: F) -> A where F: FnMut(A, <W<Count> as Spec<Count>>::Item) -> A { f(a, true) }
trait Fused: Iterator {}
trait Fz<I> { type Item; fn first(&mut self) -> Option<Self::Item>; }
struct Fu<I>(I);
impl<I: Iterator> Fz<I> for Fu<I> {
    type Item = <I as Iterator>::Item;
    default fn first(&mut self) -> Option<Self::Item> { self.0.next() }
}
impl<I: Fused> Fz<I> for Fu<I> {
    fn first(&mut self) -> Option<Self::Item> { self.0.next() }
}
mod private {
    pub trait SolTypeValue<T: super::SolType> { fn stv(&self) -> T::Token<'_>; }
}
trait SolType { type RustType; type Token<'a>; }
trait EventTopic: SolType { fn encode_topic(rust: &Self::RustType) -> u8; }
struct Address;
struct Addr;
impl SolType for Address { type RustType = Addr; type Token<'a> = u8; }
impl private::SolTypeValue<Address> for Addr { fn stv(&self) -> u8 { 0 } }
impl EventTopic for Address {
    fn encode_topic(rust: &Self::RustType) -> u8 { crate::private::SolTypeValue::<Self>::stv(rust) }
}
fn stv_named(rust: &Addr) -> u8 { private::SolTypeValue::<Address>::stv(rust) }
fn stv_bad(rust: &Count) -> u8 { private::SolTypeValue::<Address>::stv(rust) }
fn dbl(x: &u8) -> u8 { *x }
struct It(core::iter::Map<core::slice::Iter<'static, u8>, fn(&u8) -> u8>);
fn mk_it(xs: &'static [u8]) -> It { It(xs.iter().map(dbl)) }
fn mk_map(xs: &'static [u8]) -> core::iter::Map<core::slice::Iter<'static, u8>, fn(&u8) -> u8> { xs.iter().map(dbl) }
fn mk_map_bad(xs: &'static [u8]) -> core::iter::Map<core::slice::Iter<'static, u8>, fn(&u8) -> u16> { xs.iter().map(dbl) }
struct Guard<T, F> where F: FnMut(&mut T) { dropfn: F, value: T }
fn guard<T, F>(value: T, dropfn: F) -> Guard<T, F> where F: FnMut(&mut T) { Guard { dropfn, value } }
impl<T, F> core::ops::Deref for Guard<T, F> where F: FnMut(&mut T) {
    type Target = T;
    fn deref(&self) -> &T { &self.value }
}
impl<T, F> core::ops::DerefMut for Guard<T, F> where F: FnMut(&mut T) {
    fn deref_mut(&mut self) -> &mut T { &mut self.value }
}
struct Table { items: usize }
impl Table { fn reset(&mut self) { self.items = 0; } }
fn guard_field(t: &mut Table) -> usize { let g = guard(t, |t| t.reset()); g.items }
fn guard_method(t: &mut Table) { let mut g = guard(t, |t| t.reset()); g.reset() }
fn guard_bad(t: &mut Table) -> usize { let g = guard(t, |t| t.reset()); g.nope }
const ENC: &str = "A128GCM";
const FIVE: u8 = 5;
fn str_const(s: &str) -> u8 { match s { ENC => 1, _ => 0 } }
fn u8_const(x: &u8) -> u8 { match x { FIVE => 1, _ => 0 } }
fn str_const_bad(s: &&str) -> u8 { match s { ENC => 1, _ => 0 } }
unsafe extern "C" { fn ext_c() -> *const u8; fn open_v(path: *const u8, flags: i32, ...) -> i32; }
fn ext_variadic(p: *const u8) -> i32 { unsafe { open_v(p, 0, 1u32) } }
fn take_c(f: unsafe extern "C" fn() -> *const u8) -> u8 { 0 }
fn take_rust(f: fn() -> *const u8) -> u8 { 0 }
fn ext_pass() -> u8 { take_c(ext_c) }
fn ext_bad() -> u8 { take_rust(ext_c) }
enum Ctx { A(u8) }
impl Ctx {
    fn build(f: unsafe extern "C" fn() -> *const u8, n: usize) -> Result<u8, ()> { Ok(0) }
    fn mk_ctx(n: usize) -> Result<Self, ()> { Ok(Ctx::A(Ctx::build(ext_c, n)?)) }
}
"#;

/// Free fns: name, what it must say (`clean`, or one of the `|`-separated diagnostic codes),
/// and what it proves.
const FREE: [(&str, &str, &str); 17] = [
    ("ext_variadic", "clean", "a C-variadic fn of a module-level `extern \"C\"` block has the C ABI (libc's `open`)"),
    ("ext_pass", "clean", "an `extern \"C\"` fn item reifies to `unsafe extern \"C\" fn` (aws-lc-rs `AeadCtx::chacha20`)"),
    ("ext_bad", "Mismatch", "control: an `extern \"C\"` fn is no Rust-ABI `fn` pointer (E0308)"),
    ("str_const", "clean", "`ENC: &str` against a `&str` scrutinee: a constant of a reference type does not peel"),
    ("u8_const", "clean", "`FIVE: u8` against `&u8`: a constant of another type peels the reference"),
    ("str_const_bad", "Mismatch", "control: `ENC: &str` against `&&str` is refused (E0308), no peeling"),
    ("spec_where", "clean", "`<W<Count> as Spec<Count>>::Item` is the blanket impl's `I::Item`, `u8`: the specializing impl defines none"),
    ("spec_bad", "Mismatch|Unsatisfied", "control: `Item` is `u8`, so a `bool` argument is refused"),
    ("stv_named", "clean", "`SolTypeValue::<Address>::stv`: the trait's argument as written"),
    ("stv_bad", "Unsatisfied", "control: `Count: SolTypeValue<Address>` does not hold (E0277)"),
    ("mk_it", "clean", "`It(xs.iter().map(dbl))`: the field's fn pointer is `map`'s `F`, `dbl` coerces to it"),
    ("mk_map", "clean", "the return type's fn pointer is `map`'s `F`"),
    ("mk_map_bad", "Mismatch", "control: `dbl` is no `fn(&u8) -> u16` (E0308)"),
    ("guard_field", "clean", "`g.items` through `Guard`'s `Deref` whose `F: FnMut(&mut T)` waits on the closure's kind"),
    ("guard_method", "clean", "`g.reset()` through `DerefMut` the same way"),
    ("guard_bad", "NoField", "control: no field `nope` on `Table` after the derefs (E0609)"),
    ("spec_where", "clean", "(again: a second check of one body answers the same)"),
];

/// Impl members, each checked in every impl of the fixture that has it: all must be clean.
const MEMBERS: [&str; 4] = ["fold_it", "first", "encode_topic", "mk_ctx"];

/// One body lowered, with what it must say.
struct Case<B> {
    name: String,
    body: B,
    want: &'static str,
}

#[test]
fn false_bounds_through_front2() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/fb/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "fb", dir: "/fb/", root: "/fb/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut failed: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut cases = Vec::new();
    for (name, want, _proves) in FREE {
        let key = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate's root"));
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Fn(key)) {
            notes.push(format!("{name}: no head: {e:?}"));
            failed.push(name.to_string());
            continue;
        }
        match lower_fn(&names, prepared, &mut front.keys, &mut store, key) {
            Ok(body) => cases.push(Case { name: name.to_string(), body, want }),
            Err(e) => {
                notes.push(format!("{name}: does not lower: {e}"));
                failed.push(name.to_string());
            }
        }
    }
    let mut members = 0;
    for (k, _, _) in names.items(0) {
        let Some(header) = names.item_text(k).and_then(|t| t.impl_header) else { continue };
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Impl(k)) {
            notes.push(format!("impl `{header}`: no head: {e:?}"));
            failed.push(header.to_string());
            continue;
        }
        for member in MEMBERS {
            let Some(mk) = front.keys.filed_member(checker_of(k), MemberKind::Fn, member) else { continue };
            members += 1;
            match lower_member(&names, prepared, &mut front.keys, &mut store, mk) {
                Ok(body) => cases.push(Case { name: format!("<{header}>::{member}"), body, want: "clean" }),
                Err(e) => {
                    notes.push(format!("{header}::{member}: does not lower: {e}"));
                    failed.push(format!("<{header}>::{member}"));
                }
            }
        }
    }
    let mut as_stated: Vec<String> = Vec::new();
    for case in &cases {
        let view = case.body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = case.body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
        let said = match check_body(&mut records, &case.body, &scope) {
            Ok(done) if done.diags.is_empty() => "clean".to_string(),
            Ok(done) => format!("Ok with {} diagnostics", done.diags.len()),
            Err(CheckFail::Diags { diags, .. }) => format!("diagnostics {diags:?}"),
            Err(other) => format!("{other:?}"),
        };
        let ok = if case.want == "clean" { said == "clean" } else { case.want.split('|').any(|w| said.contains(w)) };
        if ok {
            as_stated.push(case.name.clone());
        } else {
            let brief: String = said.chars().take(700).collect();
            notes.push(format!("{}: want {}, said {brief}", case.name, case.want));
            failed.push(case.name.clone());
        }
    }
    notes.push(format!("as stated: {as_stated:?}"));
    // `fold_it` in both `Spec` impls, `first` in both `Fz` impls, `encode_topic` and `mk_ctx`
    // in one each.
    assert_eq!(members, 6, "every impl member was found\n{}", notes.join("\n"));
    assert!(failed.is_empty(), "{} bodies did not check as stated: {failed:?}\n{}", failed.len(), notes.join("\n"));
    println!("{}", notes.join("\n"));
}
