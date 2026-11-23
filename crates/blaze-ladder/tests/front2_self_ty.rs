//! `Self` in an impl of a type that is no struct, enum or union, and value paths through an
//! associated type, on front2 with the real core. tempo census 13: 1,043 of 1,052 "diag E0425"
//! bodies, nearly all in tempo_precompiles: `Self::from_word(..)` and `Self::BYTES` in
//! `impl<T: Packable> Storable for T`, `Self::Handler::new(..)` in `impl StorableType for
//! [T; N]`, `Self::Error::out_of_gas()` in a trait's provided method (storage_credits
//! accounting.rs), and serde's `D::Error::custom(..)`. tempo compiles under the pinned
//! compiler, so each such body here must check clean.
//!
//! The Reference, "Paths", "`Self`" (`paths.qualifiers.type-self.impl`): "In an
//! implementation, it refers to the type being implemented."; (`.trait`): "In a trait
//! definition, it refers to the type implementing the trait." "Trait and lifetime bounds"
//! (`bound.satisfaction`), given `Ty: Trait`: "In the body of a generic function, methods from
//! `Trait` can be called on `Ty` values. Likewise associated constants on the `Trait` can be
//! used. Associated types from `Trait` can be used." and "`trait A { type B: Copy; }` is
//! equivalent to `trait A where Self::B: Copy { type B; }`". The refusals are the compiler's
//! error index's: E0221 "An attempt was made to retrieve an associated type, but the type was
//! ambiguous." (two bounds declare it); E0599 for a function no candidate has.
//!
//! The world answers `Self` of such an impl with `Def::SelfTy` (front2/world_paths.rs `path`);
//! the checker's side is blaze-typeck rules/self_path.rs, its fixture form
//! suite/self_ty_paths.rs. Without either, every "clean" body here says `UnresolvedPath`.
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_panics.rs.

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

const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
trait Mk {
    const MKC: u16;
    fn mk() -> Self;
}
impl Mk for u16 {
    const MKC: u16 = 7;
    fn mk() -> Self { 0 }
}
trait Word {
    const BYTES: usize;
    fn from_word(w: u64) -> Self;
}
trait Stor {
    type Handler: Mk;
    fn a1(w: u64) -> Self;
    fn a2() -> usize;
    fn a3() -> u16;
}
impl<T: Word> Stor for T {
    type Handler = u16;
    fn a1(w: u64) -> Self { Self::from_word(w) }
    fn a2() -> usize { Self::BYTES }
    fn a3() -> u16 { Self::Handler::mk() }
}
trait Tr {
    type Out: Mk;
    const K: u8;
    fn f() -> u8;
    fn a1() -> u8;
    fn a2() -> u8;
    fn a3() -> u16;
}
impl Tr for [u8; 4] {
    type Out = u16;
    const K: u8 = 1;
    fn f() -> u8 { 1 }
    fn a1() -> u8 { Self::f() }
    fn a2() -> u8 { Self::K }
    fn a3() -> u16 { Self::Out::MKC }
}
impl Tr for u32 {
    type Out = u16;
    const K: u8 = 1;
    fn f() -> u8 { 1 }
    fn a1() -> u8 { Self::f() }
    fn a2() -> u8 { Self::K }
    fn a3() -> u16 { Self::Out::mk() }
}
impl<A, B> Tr for (A, B) {
    type Out = u16;
    const K: u8 = 1;
    fn f() -> u8 { 1 }
    fn a1() -> u8 { Self::f() }
    fn a2() -> u8 { Self::K }
    fn a3() -> u16 { Self::Out::MKC }
}
impl<T> Tr for &T {
    type Out = u16;
    const K: u8 = 1;
    fn f() -> u8 { 1 }
    fn a1() -> u8 { Self::f() }
    fn a2() -> u8 { Self::K }
    fn a3() -> u16 { Self::Out::mk() }
}
trait OutOfGas {
    fn out_of_gas() -> Self;
}
trait Backend {
    type Error: OutOfGas;
    fn charge() -> Self::Error { Self::Error::out_of_gas() }
}
trait De {
    type Error: Mk;
}
trait De2 {
    type Error;
}
fn g1<D: De>() -> D::Error { D::Error::mk() }
fn g2<D: De>() -> u16 { D::Error::MKC }
fn g3<D: De>() -> u16 { <D as De>::Error::MKC }
fn g4<D: De + De2>() -> u16 { D::Error::MKC }
fn g5<D: De>() -> u16 { D::Error::nope() }
";

/// The free bodies: what each must say (`clean`, or a word its diagnostics hold) and what it
/// proves.
const FREE: [(&str, &str, &str); 5] = [
    ("g1", "clean", "`D::Error::mk()`: a function of a parameter's associated type, through the declaration's bound (serde's `D::Error::custom`)"),
    ("g2", "clean", "`D::Error::MKC`: a constant the same way"),
    ("g3", "clean", "`<D as De>::Error::MKC`: the qualified form of the same path"),
    ("g4", "AmbiguousAssocTyInBounds", "`D::Error` where `De` and `De2` both declare `Error`: E0221"),
    ("g5", "NoMethod", "`D::Error::nope()`: no bound of `Error` has `nope`: E0599"),
];

/// The members `a1`, `a2`, `a3` of every impl must check clean: `Self::f()` / `Self::from_word(w)`,
/// `Self::K` / `Self::BYTES`, `Self::Out::..` / `Self::Handler::mk()`.
const MEMBERS: [&str; 3] = ["a1", "a2", "a3"];

/// The impls of `MAIN` whose members are checked: a blanket impl, an array, a primitive, a
/// tuple, a reference (`impl Mk for u16` has none of the members).
const IMPLS: usize = 5;

/// One body lowered, with what it must say.
struct Case<B> {
    name: String,
    body: B,
    want: &'static str,
}

#[test]
fn self_of_any_type_and_assoc_type_paths_through_front2() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/sty/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "sty", dir: "/sty/", root: "/sty/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    // The evaluator: core's inherent impls of `u32` and `u16` have heads whose array lengths
    // name constants, and `Self::f()` in `impl Tr for u32` reads them (inherent items first).
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
    // The trait's provided method: `Self::Error::out_of_gas()` where `Self` is the trait's own
    // parameter and `Error` the trait's own associated type.
    let backend = names::path(&names, names.root(0), Namespace::Types, Written { kind: PathKind::Plain, segments: &["Backend"] }, Alloc::global())
        .ok()
        .and_then(|f| f.whole(1))
        .map(|d| d.key)
        .expect("`Backend` from the crate's root");
    let mut provided = 0;
    for want in [Want::Head(backend), Want::Members(backend)] {
        if let Err(e) = store.ensure(&names, &mut front.keys, want) {
            notes.push(format!("trait `Backend`: no head: {e:?}"));
            failed.push("Backend".to_string());
        }
    }
    if let Some(mk) = front.keys.filed_member(checker_of(backend), MemberKind::Fn, "charge") {
        provided += 1;
        match lower_member(&names, prepared, &mut front.keys, &mut store, mk) {
            Ok(body) => cases.push(Case { name: "Backend::charge".to_string(), body, want: "clean" }),
            Err(e) => {
                notes.push(format!("Backend::charge: does not lower: {e}"));
                failed.push("Backend::charge".to_string());
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
        let ok = if case.want == "clean" { said == "clean" } else { said.contains(case.want) };
        if ok {
            as_stated.push(case.name.clone());
        } else {
            // The check script prints a few lines: each failure in brief, the first one longer.
            let most = if failed.is_empty() { 600 } else { 90 };
            let brief: String = said.chars().take(most).collect();
            notes.push(format!("{}: want {}, said {brief}", case.name, case.want));
            failed.push(case.name.clone());
        }
    }
    notes.push(format!("as stated: {as_stated:?}"));
    assert_eq!(members, IMPLS * MEMBERS.len(), "every impl member was found\n{}", notes.join("\n"));
    assert_eq!(provided, 1, "the trait's provided method was found\n{}", notes.join("\n"));
    assert!(failed.is_empty(), "{} bodies did not check as stated: {failed:?}\n{}", failed.len(), notes.join("\n"));
}
