//! Piece L5 (D148, docs/v2/plan/design/flow.md "Lifetimes are produced once and carried beside
//! the types"): a closure's signature regions come from the clause that expected it
//! (`Checked::closures` column 4, `SigFrom`), read from the head record's clause column by the
//! flow (blaze-flow-lower closure.rs `sig_slots`). A slot the clause's binder binds is a
//! universal region of the closure body; every other is the owner's, a position of the closure
//! value after its captures, whose relations are owner constraints (`Need::Sig`).
//!
//! Controls first:
//! - `control_conflict`: a variable assigned while a closure that captured it by reference is
//!   still live is E0506 at the assignment (the capture's loan lives while the closure VALUE
//!   is: the later call reads the value). `control_ended`, the same with the call first, is
//!   clean: the finding is the loan's liveness, not the capture.
//! - `local_return`: a closure returning a reference to its own local is refused.
//! - `hr_store`: a closure expected as `FnMut(&T)` (its input bound by the clause's binder)
//!   storing its argument into a captured `&mut Vec<&T>` is refused, never clean.
//! - `iter_projection` and `user_projection` are clean: the owner positions related through the
//!   call's type arguments (`map`'s clause binds nothing), and the binder's one variable shared
//!   by the input and the output (`impl FnMut(&T) -> &U`).
//! - `projection_bad`: the same relation, where the owner's signature does not give it, is the
//!   owner's `outlives` finding.
//! - Planted: with no closure requirement filed (`FactsPlant::NoClosureRequirements`) the clean
//!   projection is no longer clean (the body's relation is then a refusal), so the clean verdict
//!   is the requirement's.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::FactsPlant;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
fn main() {}

struct Entry { name: String }

fn control_conflict() -> u8 {
    let mut v = 0u8;
    let read = || v;
    v = 1;
    read()
}

fn control_ended() -> u8 {
    let mut v = 0u8;
    let read = || v;
    let previous = read();
    v = 1;
    previous ^ v
}

fn local_return() -> u8 {
    let make = || {
        let s = 1u8;
        &s
    };
    *make()
}

fn each<T>(xs: &[T], mut f: impl FnMut(&T)) {
    for x in xs {
        f(x);
    }
}

fn hr_store<'a>(xs: &[Entry], out: &mut Vec<&'a Entry>) {
    each(xs, |x| out.push(x));
}

fn iter_projection(xs: &[Entry]) -> Vec<&str> {
    xs.iter().map(|f| f.name.as_str()).collect::<Vec<_>>()
}

fn project<T, U: ?Sized>(xs: &[T], mut f: impl FnMut(&T) -> &U) -> Vec<&U> {
    let mut out = Vec::new();
    for x in xs {
        out.push(f(x));
    }
    out
}

fn user_projection(xs: &[Entry]) -> Vec<&str> {
    project(xs, |f| f.name.as_str())
}

fn apply<T, U, F: FnOnce(T) -> U>(x: T, f: F) -> U { f(x) }

fn projection_bad<'a, 'b>(entry: &'a Entry) -> &'b str {
    apply(entry, |e| e.name.as_str())
}
"#;

/// What a body's check must give.
#[derive(Clone, Copy, Debug)]
enum Want {
    Clean,
    /// A finding of this code (and, for the first control, at the assignment).
    Finding(&'static str),
    /// Not clean: a finding or a refusal, never a pass.
    NotClean,
}

#[test]
fn closure_signature_regions_from_the_expecting_clause() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/l5/src/main.rs".to_owned(), SOURCE.to_owned())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("Rust 2024");
    let user = UserCrate { name: "l5", dir: "/l5/", root: "/l5/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));

    let bodies = world.bodies(alloc);
    let find = |name: &str| {
        bodies
            .iter()
            .find_map(|&(body, _)| {
                let BodyRef::Free(key) = body else { return None };
                let item = names.item_text(key)?;
                (item.name == Some(name)).then_some((body, item.text))
            })
            .unwrap_or_else(|| panic!("missing fixture body {name}"))
    };
    for (name, want) in [
        // Controls first.
        ("control_conflict", Want::Finding("E0506")),
        ("control_ended", Want::Clean),
        ("local_return", Want::NotClean),
        ("hr_store", Want::NotClean),
        ("iter_projection", Want::Clean),
        ("user_projection", Want::Clean),
        // `apply` ties two owner positions (`T` and `U`), so the soundness guard turns the
        // `outlives` finding into `Stricter(ClosureSigTies)` (never accepted).
        ("projection_bad", Want::NotClean),
    ] {
        let (body, text) = find(name);
        let (types, verdict) = world.check_borrows(body, alloc);
        assert!(types.is_clean(), "{name} types: {types:?}");
        match want {
            Want::Clean => {
                assert!(verdict.refused.is_none(), "{name}: refused {:?}", verdict.refused);
                assert!(verdict.is_clean(), "{name}: {:?}", verdict.findings);
            }
            Want::Finding(code) => {
                assert!(verdict.refused.is_none(), "{name}: refused {:?}", verdict.refused);
                assert!(
                    verdict.findings.iter().any(|d| {
                        let site = text.get(d.start as usize..d.end as usize);
                        d.code == code && (name != "control_conflict" || site.is_some_and(|s| s.contains('v')))
                    }),
                    "{name}: expected {code}, got {:?}",
                    verdict.findings
                );
            }
            Want::NotClean => {
                assert!(verdict.refused.is_some() || !verdict.is_clean(), "{name}: passed, a soundness hole");
            }
        }
        println!("{name}: {want:?} refused={:?} findings={:?}", verdict.refused, verdict.findings);
    }

    // Planted: the clean projections are clean BECAUSE their relation is filed as the owner's
    // requirement; with none filed they are not.
    for name in ["iter_projection", "user_projection"] {
        let (body, _) = find(name);
        let (types, verdict) = world.check_borrows_planted(body, FactsPlant::NoClosureRequirements, alloc);
        assert!(types.is_clean(), "{name} types: {types:?}");
        if name == "iter_projection" {
            assert!(verdict.refused.is_some() || !verdict.is_clean(), "{name}: clean with no requirement filed: the control does not bite");
        }
        println!("{name} planted NoClosureRequirements: refused={:?} findings={:?}", verdict.refused, verdict.findings);
    }
}
