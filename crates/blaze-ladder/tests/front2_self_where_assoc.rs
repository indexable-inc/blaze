//! `Self::X` in a trait's member through a bound on `Self` in scope that is not a supertrait
//! (front2/types.rs `trait_self_assoc`, front2/preds.rs `param_bounds`). The Reference, "Paths",
//! "`Self`", and "Paths in types": `Self::X` is the shorthand `<Self as Tr>::X` for the one trait
//! among the bounds on `Self` in scope that declares `X`: the trait itself with its supertraits,
//! and the where clauses on `Self` of the trait and of the member. tap 1.0.1 (in tempo's tree):
//! `trait TryConv where Self: Sized { fn try_conv<T>(self) -> Result<T, Self::Error> where
//! Self: TryInto<T> { .. } }`, whose head was refused "`Self::Error` names no associated type of
//! the trait or its supertraits" (5 fns of tempo's census 7). Two distinct traits declaring the
//! name are ambiguous (rustc's E0221). Needs the installed toolchain under
//! `$HOME/.blaze/v2/toolchain`, as tests/front2_bounds.rs does.
//!
//! What it prints when right:
//! `try_conv: signature made; no_where: refused: ..names no associated type..; two: refused: AssocAmbiguous(..); use_try: checked clean`

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, Records, check_body};
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
trait TryConv where Self: Sized {
    fn try_conv<T>(self) -> Result<T, Self::Error> where Self: core::convert::TryInto<T>, T: Sized {
        core::convert::TryInto::<T>::try_into(self)
    }
}
impl<T> TryConv for T {}
fn use_try(x: u8) -> Result<u16, <u8 as core::convert::TryInto<u16>>::Error> { x.try_conv::<u16>() }
trait NoWhere: Sized {
    fn no_where<T>(self) -> Result<T, Self::Error>;
}
trait HasX { type X; }
trait HasX2 { type X; }
trait Two: HasX {
    fn two(self) -> Self::X where Self: HasX2;
}
";

#[test]
fn self_assoc_through_where_clauses_on_self() {
    let alloc = Alloc::global();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/swa/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "swa", dir: "/swa/", root: "/swa/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let root = names.root(0);
    let item = |ns: Namespace, name: &'static str| {
        names::path(&names, root, ns, Written { kind: PathKind::Plain, segments: &[name] }, alloc)
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"))
    };
    let mut line = Vec::new();

    // The trait's members: `try_conv` has its signature, the other two are refused by name.
    let mut gaps: Vec<Option<String>> = Vec::new();
    for (trait_, fn_) in [("TryConv", "try_conv"), ("NoWhere", "no_where"), ("Two", "two")] {
        let t = item(Namespace::Types, trait_);
        store.ensure(&names, &mut front.keys, Want::Members(t)).unwrap_or_else(|e| panic!("`{trait_}`'s members: {e:?}"));
        let mk = front.keys.filed_member(checker_of(t), MemberKind::Fn, fn_).unwrap_or_else(|| panic!("`{trait_}::{fn_}` filed"));
        gaps.push(store.fn_gaps.get(&mk).map(|w| format!("{w:?}")));
    }
    let [try_conv, no_where, two] = <[Option<String>; 3]>::try_from(gaps).expect("three members");
    assert_eq!(try_conv, None, "`Self::Error` through the member's `where Self: TryInto<T>` is `<Self as TryInto<T>>::Error`");
    line.push("try_conv: signature made".to_string());

    // Control: without a bound on `Self` declaring `Error`, the name is not found. A lookup that
    // took any trait declaring `Error` would make this signature.
    let gap = no_where.expect("`NoWhere::no_where` has no signature");
    assert!(gap.contains("names no associated type"), "the refusal names the lookup: {gap}");
    line.push(format!("no_where: refused: {gap}"));

    // Control: the supertrait `HasX` and the member's `where Self: HasX2` both declare `X`:
    // ambiguous (E0221), never the first found.
    let gap = two.expect("`Two::two` has no signature");
    assert!(gap.contains("AssocAmbiguous"), "the refusal is the ambiguity: {gap}");
    line.push(format!("two: refused: {gap}"));

    // A caller: the method's signature is what its call is checked against.
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let f = item(Namespace::Values, "use_try");
    store.ensure(&names, &mut front.keys, Want::Fn(f)).unwrap_or_else(|e| panic!("use_try: no head: {e:?}"));
    let body = lower_fn(&names, prepared, &mut front.keys, &mut store, f).unwrap_or_else(|e| panic!("use_try: does not lower: {e}"));
    let view = body.lowered.view();
    let spell = Spelling::of(&view).expect("spelling");
    let scope = body.scope(&spell);
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    if let Err(e) = check_body(&mut records, &body, &scope) {
        panic!("use_try: `x.try_conv::<u16>()` must check clean: {e:?}");
    }
    line.push("use_try: checked clean".to_string());
    println!("{}", line.join("; "));
}
