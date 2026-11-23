//! A body's own trait is in scope in it: the trait of the impl the body is written in, named
//! there only by path, with no `use` of it anywhere (front2/own_trait.rs `of_member`, added to
//! the body's traits by front2/body.rs `lower_member_into`).
//!
//! The evidence (tempo-front-10: "29 diag E0599"): tempo_primitives writes
//! `impl core::hash::Hash for ConfigurableAccountSignature { fn hash<H: core::hash::Hasher>(&self,
//! state: &mut H) { self.account.hash(state); .. } }` (transaction/configurable_account/mod.rs:279;
//! the same form at tt_authorization.rs:278 and tt_signature.rs:583) in files with no `use` of
//! `Hash`, and core's prelude exports only the derive macro of that name (`pub use
//! crate::hash::macros::Hash;`, core/src/prelude/v1.rs). tempo compiles under the pinned
//! compiler, so the trait an impl implements is visible in the impl's bodies.
//!
//! The Reference, "Method call expressions": "Then, for each candidate type `T`, search for a
//! visible method with a receiver of that type in the following places: 1. `T`'s inherent
//! methods (methods implemented directly on `T`). 2. Any of the methods provided by a visible
//! trait implemented by `T`." The Reference does not say in words that an impl's own trait is
//! visible in its bodies (the evidence above is what says so), and a trait that is not visible
//! gives no methods: only a declaration or a `use` in scope, or the body's own impl, makes it
//! visible. So the same call from an inherent impl, or from an impl of ANOTHER trait, in a module that neither
//! declares nor imports the trait, is "no method" (E0599): a fix that put every trait in scope
//! would pass the first two rows and fail the last two.
//!
//! What it prints when it passes (one test):
//!   test own_trait_is_in_scope_in_its_impl ... ok
//! and, in the assertion's notes when it fails, one line per body:
//!   <impl m::Mix for Outer>::mix: clean
//!   <impl core::hash::Hash for Hashed>::hash: clean
//!   <impl Outer>::bare: diagnostics [(NoMethod, ..
//!   <impl Other for Wrong>::other: diagnostics [(NoMethod, ..
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_selfpaths.rs.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_member;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::{MemberKind, checker_of};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;

/// `Mix` is declared in `m` and never imported at the root: at the root it is in scope only in
/// the bodies of `impl m::Mix for Outer`. `Hash` is core's: the trait is not in the prelude.
const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
mod m {
    pub trait Mix {
        fn mix(&self, into: &mut u8);
    }
    pub struct Inner(pub u8);
    impl Mix for Inner {
        fn mix(&self, into: &mut u8) { *into = self.0; }
    }
}
struct Outer {
    a: m::Inner,
    b: m::Inner,
}
impl m::Mix for Outer {
    fn mix(&self, into: &mut u8) {
        self.a.mix(into);
        self.b.mix(into);
    }
}
impl Outer {
    fn bare(&self, into: &mut u8) { self.a.mix(into) }
}
trait Other {
    fn other(&self, into: &mut u8);
}
struct Wrong {
    a: m::Inner,
}
impl Other for Wrong {
    fn other(&self, into: &mut u8) { self.a.mix(into) }
}
struct Hashed {
    n: u8,
    flag: bool,
}
impl core::hash::Hash for Hashed {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.n.hash(state);
        self.flag.hash(state);
    }
}
";

/// Each member by the last word of its impl's header and its name; `true`: must check clean,
/// `false`: must be refused with the no-method error.
const MEMBERS: [(&str, &str, bool, &str); 4] = [
    ("Outer", "mix", true, "`self.a.mix(into)` in `impl m::Mix for Outer`: the impl's own trait, never imported"),
    ("Hashed", "hash", true, "tempo's form: `self.n.hash(state)` in `impl core::hash::Hash for Hashed`, no `use`"),
    ("Outer", "bare", false, "the same call in the inherent `impl Outer`: `Mix` is not in scope: E0599"),
    ("Wrong", "other", false, "the same call in `impl Other for Wrong`: the body's own trait is `Other`, not `Mix`: E0599"),
];

#[test]
fn own_trait_is_in_scope_in_its_impl() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/own/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "own", dir: "/own/", root: "/own/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
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
    let mut notes: Vec<String> = Vec::new();
    let mut bodies = Vec::new();
    let mut members = 0;
    for (k, _, _) in names.items(0) {
        let Some(header) = names.item_text(k).and_then(|t| t.impl_header) else { continue };
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Impl(k)) {
            notes.push(format!("impl `{header}`: no head: {e:?}"));
            failed.push(header.to_string());
            continue;
        }
        for (ty, member, want, proves) in MEMBERS {
            if !header.trim_end().ends_with(ty) {
                continue;
            }
            let Some(mk) = front.keys.filed_member(checker_of(k), MemberKind::Fn, member) else { continue };
            members += 1;
            match lower_member(&names, prepared, &mut front.keys, &mut store, mk) {
                Ok(b) => bodies.push((format!("<{header}>::{member}"), b, want, proves)),
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
        let ok = if *want { said == "clean" } else { said.contains("NoMethod") };
        let brief: String = said.chars().take(200).collect();
        notes.push(format!("{name}: {brief}"));
        if !ok {
            notes.push(format!("  wanted {}: {proves}", if *want { "clean" } else { "NoMethod (E0599)" }));
            failed.push(name.clone());
        }
    }
    assert_eq!(members, MEMBERS.len(), "every impl member was found\n{}", notes.join("\n"));
    assert!(failed.is_empty(), "bodies that did not say what they must: {failed:?}\n{}", notes.join("\n"));
}
