//! A trait's `Self` is `MetaSized` in its members' env unless the trait's header states a
//! sizedness (front2/self_size.rs `implied_self_size`, pushed by heads.rs `trait_members`).
//!
//! The rule, from the sources: the Reference, "Special types and traits", `Sized`: "Type
//! parameters (except `Self` in traits) are `Sized` by default"; the sized hierarchy (RFC
//! 3729) relaxes that default one level, to `MetaSized`, and only a written `PointeeSized`
//! states no size. The pinned core writes `pub trait Debug: PointeeSized` (core/src/fmt/mod.rs)
//! and `pub trait MetaSized: PointeeSized` (core/src/marker.rs): a bound every type meets is
//! written only to relax the default, which is `Self: MetaSized`.
//!
//! The evidence (port27/hello-1116/stops-3.txt, stub 6): core's `fmt::Write::write_fmt`
//! declares `trait SpecWriteFmt` and `impl<W: Write + ?Sized> SpecWriteFmt for &mut W` in its
//! body and calls `self.spec_write_fmt(args)`: `W: ?Sized` is `W: MetaSized`, so the impl
//! applies to `&mut Self` only if `Self: MetaSized` holds in the body's env. It was left out
//! with "E0599 found Some(\"&mut Param(0)\")", and bstd's `Outcome::status` with it.
//! `put_spec` below is that shape (one impl: core's second, specializing impl is another
//! matter); `meta_self` asks the clause directly. `sized_meta` is a trait stating `Sized`
//! (still clean: `Sized` holds `MetaSized`), `loose_meta` one stating `PointeeSized`, which
//! must stay refused (E0277): a fix that gave every `Self` a `MetaSized` clause fails it.
//!
//! What it prints (one line per member, failures first):
//!   Out::put_spec: clean
//!   Out::meta_self: clean
//!   Sized2::sized_meta: clean
//!   Loose::loose_meta: clean (refused with Unsatisfied)
//! Before the change: `Out::put_spec: diagnostics [(NoMethod, ..` and
//! `Out::meta_self: diagnostics [(Unsatisfied, ..`.
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_own_trait.rs.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_member;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::{MemberKind, checker_of};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "#![feature(sized_hierarchy)]
fn main(cx: &Cx) -> Result<(), Error> {
    Ok(())
}
fn meta_need<T: core::marker::MetaSized>(p: *const T) -> *const T { p }
pub trait Out {
    fn put(&mut self, b: u8) -> u8;
    fn put_spec(&mut self, b: u8) -> u8 {
        trait SpecPut { fn spec_put(self, b: u8) -> u8; }
        impl<W: Out + ?Sized> SpecPut for &mut W {
            fn spec_put(self, b: u8) -> u8 { self.put(b) }
        }
        self.spec_put(b)
    }
    fn meta_self(&self) -> *const Self { meta_need(self as *const Self) }
}
pub trait Sized2: Sized {
    fn sized_meta(&self) -> *const Self { meta_need(self as *const Self) }
}
pub trait Loose: core::marker::PointeeSized {
    fn loose_meta(&self) -> *const Self { meta_need(self as *const Self) }
}
";

/// Each member by its trait and name; `None`: must check clean; `Some(code)`: must be refused
/// with that diagnostic.
const MEMBERS: [(&str, &str, Option<&str>, &str); 4] = [
    ("Out", "put_spec", None, "core's `write_fmt` shape: a block impl for `&mut W` with `W: Out + ?Sized` applies to `&mut Self`"),
    ("Out", "meta_self", None, "`Self: MetaSized` holds in a provided method of a trait that states no sizedness"),
    ("Sized2", "sized_meta", None, "`trait Sized2: Sized`: `Self: Sized` holds `Self: MetaSized`"),
    ("Loose", "loose_meta", Some("Unsatisfied"), "`trait Loose: PointeeSized` states no size: `Self: MetaSized` is E0277"),
];

#[test]
fn a_traits_self_is_meta_sized_unless_stated() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/selfsize/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "selfsize", dir: "/selfsize/", root: "/selfsize/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
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
    for (tr, member, want, proves) in MEMBERS {
        let name = format!("{tr}::{member}");
        let key = names::path(&names, names.root(0), Namespace::Types, Written { kind: PathKind::Plain, segments: &[tr] }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("the trait `{tr}` at the crate's root"));
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Members(key)) {
            notes.push(format!("{name}: no members: {e:?}"));
            failed.push(name);
            continue;
        }
        let Some(mk) = front.keys.filed_member(checker_of(key), MemberKind::Fn, member) else {
            notes.push(format!("{name}: not filed"));
            failed.push(name);
            continue;
        };
        let body = match lower_member(&names, prepared, &mut front.keys, &mut store, mk) {
            Ok(b) => b,
            Err(e) => {
                notes.push(format!("{name}: does not lower: {e}"));
                failed.push(name);
                continue;
            }
        };
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        let said = match check_body(&mut records, &body, &scope) {
            Ok(done) if done.diags.is_empty() => "clean".to_string(),
            Ok(done) => format!("Ok with {} diagnostics", done.diags.len()),
            Err(CheckFail::Diags { diags, .. }) => format!("diagnostics {diags:?}"),
            Err(other) => format!("{other:?}"),
        };
        let (ok, line) = match want {
            None => (said == "clean", format!("{name}: {}", said.chars().take(200).collect::<String>())),
            Some(code) if said.contains(&format!("({code},")) => (true, format!("{name}: clean (refused with {code})")),
            Some(code) => (false, format!("{name}: {} (wanted {code})", said.chars().take(200).collect::<String>())),
        };
        if ok {
            notes.push(line);
        } else {
            notes.push(format!("{line}; must: {proves}"));
            failed.push(name);
        }
    }
    notes.sort_by_key(|l| l.contains(": clean"));
    for line in &notes {
        println!("{line}");
    }
    assert!(failed.is_empty(), "members that did not check as they must: {failed:?}\n{}", notes.join("\n"));
}
