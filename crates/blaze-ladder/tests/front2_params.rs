//! `T::NAME` on a type parameter, on front2 with the real core: an associated constant or
//! function found through the parameter's bounds or through a blanket impl of a trait in scope.
//! The evidence: core's `core::ptr::read` and `write_bytes` ("NoMethod on Param(0)" at
//! `T::IS_ZST`) and bstd's `P::WRITE` (census-gen-3: "13 diag E0599").
//!
//! The pinned core, library/core/src/mem/mod.rs: `pub trait SizedTypeProperties: Sized {` ..
//! `const IS_ZST: bool = size_of::<Self>() == 0;` and `impl<T> SizedTypeProperties for T {}`.
//!
//! The Reference, "Paths", "Qualified paths": "Fully qualified paths allow for disambiguating
//! the path for trait implementations and for specifying canonical paths"; "Associated Items",
//! "Associated Constants": "Associated constants are constants associated with a type";
//! "Trait and lifetime bounds": "Bounds on an item must be satisfied when using the item" and a
//! bound lets the item "use the methods and associated items of the trait" on the parameter.
//! "Method call expressions" gives the order every associated lookup keeps: "1. `T`'s inherent
//! methods (methods implemented directly on `T`). 2. Any of the methods provided by a visible
//! trait implemented by `T`. If `T` is a type parameter, methods provided by trait bounds on
//! `T` are looked up first. Then all remaining methods in scope are looked up." and "If this
//! results in multiple possible candidates, then it is an error" (E0034).
//! The Reference is silent on `T::NAME` paths taking the same order as method calls: this is
//! what rustc does (rustc_hir_typeck `probe_op` in `Mode::Path`:
//! `assemble_inherent_candidates_from_param` before `assemble_extension_candidates_for_trait`).
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_panics.rs.


use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::{lower_fn, lower_member};
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::{MemberKind, checker_of};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};


const MAIN: &str = "use core::mem::SizedTypeProperties;
fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
fn p1<T>(_x: &T) -> bool { T::IS_ZST }
trait Zst {
    const Z: bool = false;
}
impl<T> Zst for T {}
fn p2<T>() -> bool { T::Z }
trait W {
    const WRITE: bool;
    fn make() -> Self;
}
fn p3<P: W>() -> bool { P::WRITE }
fn p4<P: W>() -> P { P::make() }
struct D<P>(P);
impl<P: W> D<P> {
    fn p5(&self) -> bool { P::WRITE }
}
trait Sh {
    fn p5t(&self) -> bool;
}
impl<P: W> Sh for D<P> {
    fn p5t(&self) -> bool { P::WRITE }
}
trait A1 {
    const N: u8;
}
trait A2 {
    const N: u8;
}
fn p6<Q: A1>() -> u8 { Q::N }
fn p7<Q: A1 + A2>() -> u8 { Q::N }
trait Zb {
    const Z: bool;
}
fn p8<Q: Zb>() -> bool { Q::Z }
fn p9<Q: A1>() -> bool { Q::Z }
fn p10<Q>() -> bool where Q: W { Q::WRITE }
";

/// The free bodies, each with what it proves; `true`: must check clean, `false`: must be E0034.
const FREE: [(&str, bool, &str); 9] = [
    ("p1", true, "`T::IS_ZST`: core's blanket `impl<T> SizedTypeProperties for T {}`, the trait in scope"),
    ("p2", true, "`T::Z`: a local trait's defaulted constant through a local blanket impl"),
    ("p3", true, "`P::WRITE`: a constant through the parameter's bound (bstd's `P::WRITE`)"),
    ("p4", true, "`P::make()`: a function through the parameter's bound"),
    ("p6", true, "`Q::N`: two traits in scope with `N`, only `A1` is a bound: it is picked"),
    ("p7", false, "`Q::N`: both `A1` and `A2` are bounds: E0034"),
    ("p8", true, "`Q::Z`: the bound's `Zb::Z` comes before the blanket `Zst::Z` in scope (bounds first)"),
    ("p9", true, "`Q::Z`: no bound has `Z`: the blanket impl's trait in scope"),
    ("p10", true, "`Q::WRITE`: the bound written in a where clause"),
];

/// OWED BY THE FRONT END (2026-11-16): the bodies that name a TRAIT's constant. The world's
/// `assoc_item(trait, name)` (front2/world_items.rs) reads a trait's fns and associated types
/// and has no row for its constants (heads.rs `TraitHead` keeps `assoc` and `fns`; a
/// `Const` member is counted only for dyn compatibility), so each says "NoMethod .. found
/// Some(\"Param(0)\")" (E0599), the census's own line for `T::IS_ZST` and `P::WRITE`. Each
/// name here is asserted to say exactly that, so this list must shrink when the answer comes:
/// remove a name and the table above states what the body must say then.
/// EMPTIED 2026-11-16: heads.rs `trait_members` files a row per trait constant
/// (`TraitExtra::consts`) and `assoc_item` answers it as `AssocKind::Const`; the ten names
/// that were here (p1, p2, p3, p6, p7, p8, p9, p10, p5, p5t) now say what the table states.
const OWED: [&str; 0] = [];

/// The impl members that must check clean.
const MEMBERS: [(&str, &str); 2] = [
    ("p5", "`P::WRITE` where the bound is the IMPL's (an inherent impl)"),
    ("p5t", "`P::WRITE` where the bound is the IMPL's (a trait impl: bstd's `impl<P: Rights> fmt::Debug for Dir<P>`)"),
];

#[test]
fn param_paths_through_front2() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/prm/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "prm", dir: "/prm/", root: "/prm/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut failed: Vec<String> = Vec::new();
    let mut clean: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut bodies = Vec::new();
    for (name, want, proves) in FREE {
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
            Ok(b) => bodies.push((name.to_string(), b, want, proves)),
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
        for (member, proves) in MEMBERS {
            let Some(mk) = front.keys.filed_member(checker_of(k), MemberKind::Fn, member) else { continue };
            members += 1;
            match lower_member(&names, prepared, &mut front.keys, &mut store, mk) {
                Ok(b) => bodies.push((format!("<{header}>::{member}"), b, true, proves)),
                Err(e) => {
                    notes.push(format!("{header}::{member}: does not lower: {e}"));
                    failed.push(member.to_string());
                }
            }
        }
    }
    for (name, body, want, proves) in &bodies {
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        let said = match check_body(&mut records, body, &scope) {
            Ok(done) if done.diags.is_empty() => "clean".to_string(),
            Ok(done) => format!("Ok with {} diagnostics", done.diags.len()),
            Err(CheckFail::Diags { diags, .. }) => format!("diagnostics {diags:?}"),
            Err(other) => format!("{other:?}"),
        };
        let owed = OWED.iter().any(|o| name == o || name.ends_with(&format!("::{o}")));
        let ok = if owed {
            said.contains("NoMethod")
        } else if *want {
            said == "clean"
        } else {
            said.contains("Ambiguous")
        };
        // The check script prints twelve lines of 300 bytes: the first body that fails is
        // printed whole in pieces, the others in brief.
        if !ok && failed.is_empty() {
            let chars: Vec<char> = format!("{name}: {said}").chars().collect();
            for piece in chars.chunks(280).take(4) {
                notes.push(piece.iter().collect());
            }
        } else {
            let brief: String = said.chars().take(60).collect();
            match notes.last_mut() {
                Some(last) if last.len() < 200 && last.starts_with("; ") => last.push_str(&format!("; {name}: {brief}")),
                _ => notes.push(format!("; {name}: {brief}")),
            }
        }
        let _ = proves;
        if ok { clean.push(name.clone()) } else { failed.push(name.clone()) }
    }
    notes.push(format!("clean: {clean:?}"));
    assert_eq!(members, MEMBERS.len(), "every impl member was found\n{}", notes.join("\n"));
    assert!(failed.is_empty(), "bodies that did not check as stated: {failed:?}\n{}", notes.join("\n"));
}
