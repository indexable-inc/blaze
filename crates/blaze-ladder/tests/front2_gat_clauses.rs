//! A generic associated type's own clauses through front2 (decision D89 (b); front2/preds.rs
//! `Builder::assoc_own_clauses`, blaze-solve structural.rs's `Projection` well-formedness arm).
//! Each fixture is a fn whose BODY is checked through front2 with the real core. Needs the
//! installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_bounds.rs does.
//!
//! The Reference, "Associated Items", "Associated Types": "Associated types may include generic
//! parameters and where clauses; these are often referred to as generic associated types, or
//! GATs", declared as `type Assoc<Params> where WhereBounds;` and defined by an impl as `type
//! Assoc<Params> = Type where WhereBounds;`. "Generic parameters": a bound written on a
//! parameter is the where clause `T: Bound`. So `type Out<T: Clone>` makes `Clone` a
//! requirement of every use `<X as Tr>::Out<A>`: `A: Clone` must hold there (the language's
//! E0277), and an impl's `type Out<T: Clone> = (T, u8)` may assume it.
//!
//! A wrong fix fails one side: refusing the bound (the old `Unsupported "a bound on an
//! associated type's own parameter"`) leaves both bodies without a head; filing it and not
//! proving it checks `g` clean; filing it as an item bound of the alias (assumed, never
//! proved) checks `g` clean too.
//!
//! The line it prints: "gat clauses: clean 1, refused 1 (Unsatisfied ...)", the one
//! diagnostic of `g` in the parentheses. Needs blaze-typeck's signature well-formedness
//! (wiring/blaze-typeck/W1-10-1.md).

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
trait Tr { type Out<T: Clone>; }
impl Tr for u8 { type Out<T: Clone> = (T, u8); }
struct NotClone;
fn f(x: <u8 as Tr>::Out<u16>) { let _v = x; }
fn g(x: <u8 as Tr>::Out<NotClone>) { let _v = x; }
";

/// `<u8 as Tr>::Out<u16>` and `<u8 as Tr>::Out<NotClone>` (the Reference, "Associated
/// Types": the GAT's own clause `T: Clone` holds of `u16`, by core's primitive `Clone` impls,
/// not of `NotClone`), and the line printed. The fixture names no `String` or `Vec`: in the
/// blaze edition's prelude those are blaze's own, generic over an allocator with no default.
#[test]
fn gat_own_clauses_through_front2() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/gatc/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "gatc", dir: "/gatc/", root: "/gatc/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
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
    let mut outcome: Vec<(&str, Result<(), String>)> = Vec::new();
    for name in ["f", "g"] {
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Fn(lib(name))) {
            outcome.push((name, Err(format!("no head: {e:?}"))));
            continue;
        }
        let body = match lower_fn(&names, prepared, &mut front.keys, &mut store, lib(name)) {
            Ok(b) => b,
            Err(e) => {
                outcome.push((name, Err(format!("does not lower: {e}"))));
                continue;
            }
        };
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        let got = match check_body(&mut records, &body, &scope) {
            Ok(_) => Ok(()),
            Err(CheckFail::Diags { diags, .. }) => {
                let shown: Vec<String> = diags.iter().map(|(code, _, what)| format!("{code:?} {what}")).collect();
                Err(format!("diagnostics: {}", shown.join("; ")))
            }
            Err(other) => Err(format!("{other:?}")),
        };
        outcome.push((name, got));
    }
    let [("f", clean), ("g", refused)] = outcome.as_slice() else { panic!("two bodies, `f` then `g`: {outcome:?}") };
    assert!(clean.is_ok(), "`f(x: <u8 as Tr>::Out<u16>)` checks clean (`u16: Clone`): {clean:?}");
    // One diagnostic, E0277 (`Unsatisfied`): the signature's `WellFormed` obligation fails on
    // the own clause `NotClone: Clone` (blaze-typeck rules/control.rs `start_body`). OWED: the
    // finding's text does not yet name the failed leaf under a `WellFormed` goal (machine.rs
    // reports a non-trait root with no goal), so the text is printed, not asserted on.
    let Err(text) = refused else { panic!("`g(x: <u8 as Tr>::Out<NotClone>)` checked clean: the GAT's own clause `T: Clone` was not proved") };
    let one = text.strip_prefix("diagnostics: ").unwrap_or_else(|| panic!("`g` is refused by a diagnostic, not by {text}"));
    assert!(!one.contains("; "), "exactly one diagnostic for `g`: {one}");
    assert!(one.starts_with("Unsatisfied"), "`g`'s one diagnostic is E0277 (`Unsatisfied`): {one}");
    println!("gat clauses: clean 1, refused 1 ({one})");
}
