//! Two forms of an associated type written in a fn's head, through front2 with the real core
//! (tempo census 14: 9 bodies refused "type arguments on a parameter's associated type
//! (`T::X<A>`)", 7 refused "an associated item constraint"). Needs the installed toolchain
//! under `$HOME/.blaze/v2/toolchain`, as tests/front2_gat_clauses.rs does.
//!
//! 1. `A::Out<u16>` with `A: Tr` and `trait Tr { type Out<T>; }`. The Reference, "Paths",
//!    "Paths in types", and "Associated Items", "Associated Types" (generic associated types):
//!    a type parameter's shorthand `A::Out<u16>` is `<A as Tr>::Out<u16>`, `Tr` the one bound
//!    of `A` declaring `Out`, with the GAT's own argument `u16`. `f` returns its `A::Out<u16>`
//!    argument as `<A as Tr>::Out<u16>`: clean only when both are the same projection, own
//!    argument included (front2/types.rs `path_ty` -> `param_assoc` -> `assoc_with_args`).
//!    Without the fix `f` has no head ("type arguments on a parameter's associated type").
//! 2. `impl Src<Error: Clone>` in a parameter. The Reference, "Trait and lifetime bounds"
//!    (associated item constraints, stable since 1.79): `impl Tr<Assoc: Bounds>` is `impl Tr`
//!    and `<P as Tr>::Assoc: Bounds`. `g` clones the `Error` its argument gives: clean only when
//!    the clause `<P as Src>::Error: Clone` is on record (preds.rs `Builder::bound`, `nested`).
//!    Without the fix `g` has no head ("an associated item constraint": heads.rs `anon_params`
//!    walked the bound's path as a type; front2/bound_kids.rs).
//! 3. `impl Gat<Out<u8>: Clone>`, an associated type bound with type arguments on the
//!    associated type, a form not built: refused by name, "an associated type bound with type
//!    or const arguments (`Item<T>: Bound`)" (const_args_in_bounds.rs), never taken as `Out:
//!    Clone`.
//!
//! The line it prints: "assoc forms: clean 2 (f, g), refused 1 (h: an associated type bound with
//! type or const arguments)".

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "fn main() {}
trait Tr { type Out<T>; }
impl Tr for u8 { type Out<T> = (T, u8); }
fn f<A: Tr>(x: A::Out<u16>) -> <A as Tr>::Out<u16> { x }
trait Src { type Error; fn err(&self) -> Self::Error; }
fn g(s: impl Src<Error: Clone>) { let e = s.err(); let _d = e.clone(); }
trait Gat { type Out<T>; }
fn h(s: impl Gat<Out<u8>: Clone>) { let _v = s; }
";

/// The refusal of form 3, as const_args_in_bounds.rs names it.
const REFUSED: &str = "an associated type bound with type or const arguments (`Item<T>: Bound`)";

#[test]
fn assoc_forms_in_heads() {
    let heap = blaze_own::testing::heap();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let (dir, root) = ("/assocf/", "/assocf/src/main.rs");
    let files = [(String::from(root), String::from(MAIN))];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "assocf", dir, root, files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let lib = |name: &'static str| {
        names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, heap.alloc())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"))
    };
    let prepared = Prepares::new_in(heap.alloc());
    let mut buckets = None;
    let mut outcome: Vec<(&str, Result<(), String>)> = Vec::new();
    for name in ["f", "g", "h"] {
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
    let [("f", f), ("g", g), ("h", h)] = outcome.as_slice() else { panic!("three bodies, `f`, `g`, `h`: {outcome:?}") };
    assert!(f.is_ok(), "`fn f<A: Tr>(x: A::Out<u16>) -> <A as Tr>::Out<u16> {{ x }}` checks clean (one projection, own argument kept): {f:?}");
    assert!(g.is_ok(), "`fn g(s: impl Src<Error: Clone>)` checks clean (`<P as Src>::Error: Clone` on record): {g:?}");
    let Err(text) = h else { panic!("`fn h(s: impl Gat<Out<u8>: Clone>)` checked clean: a form not built was taken") };
    assert!(text.starts_with("no head: "), "`h` is refused at its head, not by {text}");
    assert!(text.contains(REFUSED), "`h`'s head is refused by name ({REFUSED}): {text}");
    println!("assoc forms: clean 2 (f, g), refused 1 (h: an associated type bound with type or const arguments)");
}
