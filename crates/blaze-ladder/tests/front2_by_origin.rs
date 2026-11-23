//! Every front2 World answer goes by what its key stands for (front2/by_origin.rs): a kind no
//! want makes is refused by name, never asked for as another kind, and a known key is never
//! reported unknown. The unknown-key lane's class (2026-11-16): an answer that handled some
//! kinds and let the rest fall through stopped forty-eight std rungs.
//!
//! What each part asserts, with the answer before the fix (the control):
//! 1. `want_of_item` (fill.rs): a `static` is `Want::Fn`, a type alias `Want::Head`, a module
//!    `FillError::NoHead(.., Mod)`. Before: the module was `Ok(Want::Fn(module))` (`_ =>
//!    Want::Fn(lib)`), which the head store then refused as "a module".
//! 2. `want_of_key` (fill.rs): a member filed under a member is `FillError::MemberOfMember`
//!    naming both and the container's kind. Before: `Err(UnknownKey(container))`, though the
//!    container is in the map. An anonymous constant is `FillError::AnonConst`. Before:
//!    `Ok(Want::Fn(root module))`.
//! 3. `predicates` of an anonymous constant with no row (by_origin `predicates_unmade`):
//!    `KindGap::AnonUnfiled`. Before: the world's `None => self.want(item)` gave
//!    `Want::Fn(root module)`.
//! 4. `const_params` of an ADT whose head is not made (by_origin `const_params_of`): its
//!    want, `Want::Adt(Buf)`; once made, its one const parameter at index 0. Before: `[]`, "no
//!    const parameters", for a head not made. The Reference, "Generic parameters": "Const
//!    generic parameters" are declared in the item's generics (`struct Buf<const N: usize>`).
//! 5. A variant index past the made ADT's variants (by_origin `ctor_def`, the world's `ctor`):
//!    `KindGap::NoVariant` naming the ADT, the index and the count. Before: the gap
//!    `Fill(UnknownKey(Two))`, printed "the checker key .. is in no row of the front end's key
//!    map, and its crate's records hold no item under it (gap Fill/UnknownKey)", for an ADT
//!    whose head is made. The Reference, "Enumerations": `enum Two { A, B }` has two variants.
//!
//! Line it prints: `by origin: static Fn, alias Head, module NoHead; member of member refused;
//! anon AnonConst, anon preds AnonUnfiled; Buf const params Want(Adt) then [0]; Two variant 1
//! Ctor, variant 5 of 2 NoVariant` (one line).
//!
//! It needs no toolchain: the program is `#![no_core]` (as tests/front2_fn_attrs.rs).
//! NOT RUN by the writer (a write-only lane).
#![feature(allocator_api)]

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::by_origin::{KeyClass, KindGap, Refusal, Unmade, const_params_of, ctor_def, predicates_unmade};
use blaze_ladder::front2::fill::{FillError, want_of_item, want_of_key};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::{MemberKind, checker_of};
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::NewFront;
use blaze_scope::names::{self, Written};
use blaze_surface::{ItemKind as SurfaceKind, Namespace, PathKind};
use blaze_typeck::Def;

const PROGRAM: &str = r#"#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "pointee_sized"]
trait PointeeSized {}

#[lang = "meta_sized"]
trait MetaSized: PointeeSized {}

#[lang = "sized"]
trait Sized: MetaSized {}

mod inner {}

type Alias = u32;

static S: u32 = 0;

struct Buf<const N: usize>(u32);

enum Two {
    A,
    B,
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    0
}
"#;

#[test]
fn every_world_answer_goes_by_what_its_key_is() {
    let heap = blaze_own::testing::heap();
    let files = [SourceFile { path: "src/main.rs", text: PROGRAM }];
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let mut front = Front2::new(&names, &frame, &files, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let root = names.root(0);
    let find = |ns: Namespace, name: &str| {
        names::path(&names, root, ns, Written { kind: PathKind::Plain, segments: &[name] }, heap.alloc())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"))
    };
    let (inner, alias, s, buf, two) =
        (find(Namespace::Types, "inner"), find(Namespace::Types, "Alias"), find(Namespace::Values, "S"), find(Namespace::Types, "Buf"), find(Namespace::Types, "Two"));
    let mut parts: [&str; 7] = [""; 7];

    // 1. want_of_item by the item's outline detail.
    assert_eq!(want_of_item(&names, s), Ok(Want::Fn(s)), "a static's head is made by `Want::Fn`");
    assert_eq!(want_of_item(&names, alias), Ok(Want::Head(alias)), "a type alias's whole record is its head");
    // The crate root's key has surely no outline entry of its own: its kind is the module
    // tree's (world-by-origin-2: it and `inner` were refused `NoDetail` when told by detail).
    match want_of_item(&names, root) {
        Err(FillError::NoHead(k, SurfaceKind::Module)) if k == root => {}
        other => panic!("the crate root is a module, which has no head; got {other:?} (world-by-origin-1: Err(NoDetail(root)); before it: Ok(Fn(root)))"),
    }
    match want_of_item(&names, inner) {
        Err(FillError::NoHead(k, SurfaceKind::Module)) if k == inner => parts[0] = "static Fn, alias Head, module NoHead",
        other => panic!("a module has no head; got {other:?} (world-by-origin-1: Err(NoDetail(module)); before it: Ok(Fn(module)))"),
    }

    // 2. want_of_key: a member of a member, and an anonymous constant.
    let two_ck = front.keys.item(two).unwrap_or_else(|c| panic!("Two's key: {c}"));
    let outer = front.keys.member(two_ck, MemberKind::Type, "T").unwrap_or_else(|c| panic!("the outer member: {c}"));
    let nested = front.keys.member(outer, MemberKind::Const, "C").unwrap_or_else(|c| panic!("the nested member: {c}"));
    match want_of_key(&names, &front.keys, nested) {
        Err(FillError::MemberOfMember { member, container, class: KeyClass::MemberType }) if member == nested && container == outer => {
            parts[1] = "member of member refused"
        }
        other => panic!("a member of a member is refused by name; got {other:?} (before the fix: Err(UnknownKey(container)))"),
    }
    let root_ck = front.keys.item(root).unwrap_or_else(|c| panic!("the root module's key: {c}"));
    let anon = front.keys.member(root_ck, MemberKind::Anon, "usize 1 + 1").unwrap_or_else(|c| panic!("the anonymous constant's key: {c}"));
    match want_of_key(&names, &front.keys, anon) {
        Err(FillError::AnonConst(k)) if k == anon => parts[2] = "anon AnonConst",
        other => panic!("no want makes an anonymous constant; got {other:?} (before the fix: Ok(Fn(root module)))"),
    }

    // 3. predicates of an anonymous constant the store holds no row for.
    match predicates_unmade(&names, &front.keys, &store, anon) {
        Unmade::Refused(Refusal::Kind(KindGap::AnonUnfiled { key, .. })) if key == anon => parts[3] = "anon preds AnonUnfiled",
        other => panic!("an unfiled anonymous constant's predicates are refused by name; got {other:?} (before the fix: Want(Fn(root module)))"),
    }

    // 4. const_params of an ADT: its want before its head is made, its row after.
    let buf_ck = checker_of(buf);
    match const_params_of(&names, &front.keys, &store, buf_ck) {
        Err(Unmade::Want(Want::Adt(k))) if k == buf => {}
        other => panic!("Buf's const params before its head is made are its want; got {other:?} (before the fix: Ok([]))"),
    }
    store.ensure(&names, &mut front.keys, Want::Adt(buf)).unwrap_or_else(|e| panic!("Buf's head: {e:?}"));
    match const_params_of(&names, &front.keys, &store, buf_ck) {
        Ok(params) if params.len() == 1 && params[0].0 == 0 => parts[4] = "Buf const params Want(Adt) then [0]",
        other => panic!("Buf has one const parameter, index 0; got {other:?}"),
    }

    // 5. a variant index past the made ADT's variants.
    store.ensure(&names, &mut front.keys, Want::Adt(two)).unwrap_or_else(|e| panic!("Two's head: {e:?}"));
    match ctor_def(&names, &front.keys, &store, two, 1) {
        Ok(Def::Ctor { adt, variant: 1, .. }) if adt == two_ck => parts[5] = "Two variant 1 Ctor",
        other => panic!("Two's variant 1 is `B`; got {other:?}"),
    }
    match ctor_def(&names, &front.keys, &store, two, 5) {
        Err(Unmade::Refused(Refusal::Kind(KindGap::NoVariant { adt, index: 5, variants: 2, .. }))) if adt == two_ck => parts[6] = "variant 5 of 2 NoVariant",
        other => panic!("variant 5 of a two-variant enum is refused naming the enum; got {other:?} (before the fix: Fill(UnknownKey(Two)))"),
    }

    let line = format!(
        "by origin: {}; {}; {}, {}; {}; {}, {}",
        parts[0], parts[1], parts[2], parts[3], parts[4], parts[5], parts[6]
    );
    println!("{line}");
    assert_eq!(
        line,
        "by origin: static Fn, alias Head, module NoHead; member of member refused; anon AnonConst, anon preds AnonUnfiled; Buf const params Want(Adt) then [0]; Two variant 1 Ctor, variant 5 of 2 NoVariant"
    );
}
