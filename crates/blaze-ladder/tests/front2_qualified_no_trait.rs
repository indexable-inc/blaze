//! `<T>::X`, a qualified path with no trait, on front2 with the real core (front2/types.rs
//! `qualified_no_trait`). The Reference, "Paths", "Qualified paths" and "Paths in types": with no
//! `as Trait` the self type is only bracketed, and the associated item is looked up on the type
//! as for `T::X`: for a type parameter through its bounds in scope, E0220 when none declares the
//! name, E0221 when two do. The real site: chacha20 0.10's NEON backend,
//! `macro_rules! add_counter { ($a:expr, $b:expr, $variant:ty) => { match
//! size_of::<<$variant>::Counter>() { .. } } }` in `impl<R: Rounds, V: Variant> Backend<R, V>`'s
//! `write_par_ks_blocks` (tempo t1500: 5 tests NOT RUN, "a qualified path with no trait
//! (`<T>::X`)", expanded type `<V >:: Counter`). Needs the installed toolchain under
//! `$HOME/.blaze/v2/toolchain`, as tests/front2_self_where_assoc.rs does.
//!
//! What it prints when right:
//! `keep: signature made; missing: refused: AssocNotFound..; two: refused: AssocAmbiguous..;
//! concrete: refused: ..inherent associated type..; width, direct, shorthand: checked clean`

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_member;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::{Keys, MemberKind, checker_of};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
pub trait Variant { type Counter: Copy; }
pub struct Legacy;
impl Variant for Legacy { type Counter = u32; }
pub struct Backend<V: Variant> { v: core::marker::PhantomData<V> }
macro_rules! counter_size {
    ($variant:ty) => { core::mem::size_of::<<$variant>::Counter>() };
}
impl<V: Variant> Backend<V> {
    fn width(&self) -> usize { counter_size!(V) }
    fn direct(&self) -> usize { core::mem::size_of::<<V>::Counter>() }
    fn shorthand(&self) -> usize { core::mem::size_of::<V::Counter>() }
    fn keep(c: <V>::Counter) -> <V>::Counter { c }
}
pub trait HasX { type X; }
pub trait HasX2 { type X; }
fn missing<T: HasX>() -> <T>::Missing { loop {} }
fn two<T: HasX + HasX2>() -> <T>::X { loop {} }
fn concrete() -> <Legacy>::Counter { 0 }
";

#[test]
fn qualified_path_with_no_trait_is_type_relative() {
    let alloc = Alloc::global();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/qnt/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "qnt", dir: "/qnt/", root: "/qnt/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
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

    // The inherent impl `impl<V: Variant> Backend<V>`: its head, then its members.
    let mut backend = None;
    for (k, _, _) in names.items(0) {
        let Some(header) = names.item_text(k).and_then(|t| t.impl_header) else { continue };
        if header.contains("Backend") {
            store.ensure(&names, &mut front.keys, Want::Impl(k)).unwrap_or_else(|e| panic!("impl `{header}`: no head: {e:?}"));
            backend = Some(k);
        }
    }
    let backend = backend.expect("the impl of `Backend`");
    let member = |keys: &Keys, name: &str| keys.filed_member(checker_of(backend), MemberKind::Fn, name).unwrap_or_else(|| panic!("`Backend::{name}` filed"));

    // The signature `fn keep(c: <V>::Counter) -> <V>::Counter`: `<V as Variant>::Counter` through
    // the impl header's `V: Variant`. Before the fix: "a qualified path with no trait".
    let keep = member(&front.keys, "keep");
    // An impl's fn's signature is its own want (heads.rs `Want::MemberFn`); its refusal is its
    // `fn_gaps` row.
    store.ensure(&names, &mut front.keys, Want::MemberFn(backend, keep)).unwrap_or_else(|e| panic!("`keep`'s signature: {e:?}"));
    assert_eq!(store.fn_gaps.get(&keep).map(|w| format!("{w:?}")), None, "`<V>::Counter` is `<V as Variant>::Counter`");
    line.push("keep: signature made".to_string());

    // Negative twins (rustc refuses each): no bound of `T` declares `Missing` (E0220); `HasX`
    // and `HasX2` both declare `X` (E0221). A lookup that took any trait declaring the name, or
    // the first one found, would make these heads.
    for (name, want) in [("missing", "AssocNotFound"), ("two", "AssocAmbiguous")] {
        let k = item(Namespace::Values, name);
        let e = store.ensure(&names, &mut front.keys, Want::Fn(k)).err().map(|e| format!("{e:?}")).unwrap_or_else(|| panic!("`{name}` must be refused"));
        assert!(e.contains(want), "`{name}` is refused as {want}: {e}");
        line.push(format!("{name}: refused: {e}"));
    }
    // `<Legacy>::Counter`: a concrete self type looks up an inherent associated type (rustc's
    // E0223 without `inherent_associated_types`), never `Variant`'s through the impl.
    let k = item(Namespace::Values, "concrete");
    let e = store.ensure(&names, &mut front.keys, Want::Fn(k)).err().map(|e| format!("{e:?}")).expect("`concrete` must be refused");
    assert!(e.contains("inherent associated type"), "`<Legacy>::Counter` is refused by name: {e}");
    line.push(format!("concrete: refused: {e}"));

    // The bodies: the macro-written form (the real site), the form written out, and the control
    // `V::Counter` (the shorthand that already checked): all three check clean.
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    for name in ["width", "direct", "shorthand"] {
        let mk = member(&front.keys, name);
        store.ensure(&names, &mut front.keys, Want::MemberFn(backend, mk)).unwrap_or_else(|e| panic!("`{name}`'s signature: {e:?}"));
        assert_eq!(store.fn_gaps.get(&mk).map(|w| format!("{w:?}")), None, "`{name}` has its signature");
        let body = lower_member(&names, prepared, &mut front.keys, &mut store, mk).unwrap_or_else(|e| panic!("{name}: does not lower: {e}"));
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        match check_body(&mut records, &body, &scope) {
            Ok(done) if done.diags.is_empty() => {}
            Ok(done) => panic!("{name}: Ok with {} diagnostics", done.diags.len()),
            Err(CheckFail::Diags { diags, .. }) => panic!("{name}: diagnostics {diags:?}"),
            Err(other) => panic!("{name}: {other:?}"),
        }
    }
    line.push("width, direct, shorthand: checked clean".to_string());
    println!("{}", line.join("; "));
}
