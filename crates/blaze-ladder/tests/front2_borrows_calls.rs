//! BR3: a call is related by its callee's own signature when its generic arguments carry
//! lifetimes (front2 call_generics.rs, blaze-flow-lower generic_call.rs). Before, every such
//! call of a callee whose signature writes a lifetime was refused `CallWritesLifetimes`.
//!
//! The callee `pick<'a, T>(x: &'a T, y: &u8) -> &'a T` returns what `x` borrows; its `y` is an
//! elided input lifetime of its own (the Reference, "Lifetime elision": "Each elided lifetime
//! in the parameters becomes a distinct lifetime parameter"), related to nothing in the output.
//! `T` is one type in the whole signature (the Reference, "Generic parameters"), so at
//! `pick(&ra, ..)` with `ra: &u8` the `&'t u8` of `x` is the `&'t u8` of the result.
//!
//! - `short_y`: `y` borrows `b`, which dies before the result is read: CLEAN (the result holds
//!   only `x`'s borrows). Expected line: `borrows short_y: [] refused None`.
//! - `too_short`: `x` borrows `rb` (and through `T`, `b`), both die before the result is read:
//!   the Reference, "Temporary scopes"/"References": a borrow may not outlive its referent, so
//!   ONE OR MORE findings, the first E0597 (a borrowed value goes out of scope while the borrow
//!   is live) or E0505, refused None. Expected line: `borrows too_short: ["E0597 at ..."..]
//!   refused None` (the code E0597 or E0505).
//! - `stash`: `keep<T: 'static>(x: T)` called with `&a`: the Reference, "Trait and lifetime
//!   bounds": "`T: 'a` means that all lifetime parameters of `T` outlive `'a`": a borrow of a
//!   local is not `'static`: at least one finding, refused None.
//!
//! CONTROL (fails without the fix): with front2's `Facts::call_bounds` answering `None` (so
//! lib.rs `call_done` never takes `call_by_generic_signature`), all three print `refused
//! Some(Lower(Refusal { form: CallWritesLifetimes, .. }))` and the first assertion fails.
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::BorrowVerdict;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const EDITION: &str = "blaze-2026.11";

const MAIN: &str = "fn main() {}

fn pick<'a, T>(x: &'a T, y: &u8) -> &'a T {
    let _ = y;
    x
}

fn keep<T: 'static>(x: T) {
    let _ = x;
}

fn short_y() -> u8 {
    let a = 1u8;
    let ra = &a;
    let r;
    {
        let b = 2u8;
        r = pick(&ra, &b);
    }
    **r
}

fn too_short() -> u8 {
    let a = 1u8;
    let r;
    {
        let b = 2u8;
        let rb = &b;
        r = pick(&rb, &a);
    }
    **r
}

fn stash() {
    let a = 1u8;
    keep(&a);
}
";

fn shown(item: &str, v: &BorrowVerdict<'_>) -> String {
    let found: Vec<String> = v.findings.iter().map(|d| format!("{} at {:?}", d.code, item.get(d.start as usize..d.end as usize).unwrap_or("<out of range>"))).collect();
    format!("{found:?} refused {:?}", v.refused)
}

#[test]
fn generic_calls_related_by_the_callee_signature() {
    let heap = blaze_own::testing::heap();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/br3/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "br3", dir: "/br3/", root: "/br3/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));

    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, heap.alloc()).unwrap_or_else(|e| panic!("front2: {e}"));
    let bodies = world.bodies(heap.alloc());
    let name_of = |b: &BodyRef<'_>| -> String {
        match b {
            BodyRef::Free(k) => names.item_text(*k).and_then(|t| t.name).unwrap_or("?").to_string(),
            BodyRef::Member { name, .. } => name.to_string(),
        }
    };
    let find = |n: &str| bodies.iter().find(|(b, _)| name_of(b) == n).map(|(b, _)| *b).unwrap_or_else(|| panic!("`{n}` listed"));
    let text_of = |b: BodyRef<'_>| {
        let k = match b {
            BodyRef::Free(k) => k,
            BodyRef::Member { container, .. } => container,
        };
        names.item_text(k).expect("the body's item text").text
    };

    // Was refused `CallWritesLifetimes`; now checked, and clean: `y`'s borrow is not the result's.
    let b = find("short_y");
    let (types, v) = world.check_borrows(b, heap.alloc());
    println!("borrows short_y: {}", shown(text_of(b), &v));
    assert!(types.is_clean(), "short_y's types: {types:?}");
    assert!(v.refused.is_none(), "short_y not checked: {}", shown(text_of(b), &v));
    assert!(v.findings.is_empty(), "short_y: {}", shown(text_of(b), &v));

    // The result borrows `rb` (and `b` through `T`), both dead before `**r`: found.
    let b = find("too_short");
    let (types, v) = world.check_borrows(b, heap.alloc());
    let item = text_of(b);
    println!("borrows too_short: {}", shown(item, &v));
    assert!(types.is_clean(), "too_short's types: {types:?}");
    assert!(v.refused.is_none(), "too_short not checked: {}", shown(item, &v));
    assert!(!v.findings.is_empty(), "too_short: no finding: {}", shown(item, &v));
    assert!(v.findings.iter().any(|d| d.code == "E0597" || d.code == "E0505"), "too_short: {}", shown(item, &v));

    // `T: 'static` instantiated with a borrow of a local: found.
    let b = find("stash");
    let (types, v) = world.check_borrows(b, heap.alloc());
    println!("borrows stash: {}", shown(text_of(b), &v));
    assert!(types.is_clean(), "stash's types: {types:?}");
    assert!(v.refused.is_none(), "stash not checked: {}", shown(text_of(b), &v));
    assert!(!v.findings.is_empty(), "stash: no finding: {}", shown(text_of(b), &v));
}
