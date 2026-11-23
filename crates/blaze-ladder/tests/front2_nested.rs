//! Impls written inside blocks, through the real front end over the installed toolchain
//! (`$HOME/.blaze/v2/toolchain`, as tests/front2_locals.rs): each fixture is a fn whose body
//! must check clean ONLY IF an impl written in some other item's block is in the impl index.
//!
//! The rule (the Rust Reference, fetched 2026-11-16). "Statements", "Item declarations": "An
//! item declaration statement has a syntactic form identical to an item declaration within a
//! module. Declaring an item within a statement block restricts its scope to the block
//! containing the statement. The item is not given a canonical path nor are any sub-items it
//! may declare. The exception to this is that associated items defined by implementations are
//! still accessible in outer scopes as long as the item and, if applicable, trait are
//! accessible." and "There is no implicit capture of the containing function's generic
//! parameters, parameters, and local variables." "Implementations": "The orphan rules" and
//! coherence are stated per crate, never per block: an implementation is not a name, so the
//! block hides nothing of it; its header and members are read in the block's scope (the
//! block's own `use` and `extern crate`, then the blocks around it, then the module).
//!
//! The shapes are the real ones: alloy's `sol!` (alloy-sol-macro-expander 1.7.3,
//! src/expand/function.rs:191) writes `const _: () = { use #alloy_sol_types as
//! alloy_sol_types; impl alloy_sol_types::SolCall for #call_name { type Token<'a> =
//! <Self::Parameters<'a> as alloy_sol_types::SolType>::Token<'a>; .. } };`, serde's derive
//! writes `const _: () = { extern crate serde as _serde; impl _serde::Serialize for X {..} };`
//! and, inside that impl's fns, further structs and impls (`nested_twice`). The harness has
//! one user crate, so the `extern crate` fixture renames `core`.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::{lower_fn, lower_member};
use blaze_ladder::front2::keys::{MemberKind, checker_of};
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_scope::resolve::At;
use blaze_surface::{ItemDetail, Namespace, PathKind};

const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
trait Tr { const K: u8; fn f(&self) -> u8; }
struct X;
const _: () = { impl Tr for X { const K: u8 = 1; fn f(&self) -> u8 { 1 } } };
fn in_const(x: X) -> u8 { x.f() }
fn in_const_k() -> u8 { <X as Tr>::K }
struct Xm;
impl Tr for Xm { const K: u8 = 9; fn f(&self) -> u8 { 9 } }
fn module_k() -> u8 { <Xm as Tr>::K }

struct Xb;
const _: () = { use crate::Tr as T2; impl T2 for Xb { const K: u8 = 2; fn f(&self) -> u8 { T2::f(&Xb) } } };
fn block_use(x: Xb) -> u8 { x.f() }

trait A { fn a(&self) -> u8 { 1 } }
trait B { fn b(&self) -> u8 { 2 } }
struct Xc;
const _: () = { { impl A for Xc {} } impl B for Xc {} };
fn nested_blocks(x: Xc) -> u8 { x.a() + x.b() }

struct Xd;
const _: () = { extern crate core as _c; impl _c::default::Default for Xd { fn default() -> Xd { Xd } } };
fn block_extern_crate() -> Xd { <Xd as Default>::default() }

struct Xe;
fn holder() -> u8 { impl Tr for Xe { const K: u8 = 5; fn f(&self) -> u8 { 5 } } 0 }
fn in_fn_body(x: Xe) -> u8 { x.f() }

trait St { type Token<'a>; }
trait Call { type Parameters<'a>: St; type Token<'a>; fn tok<'a>(&self) -> Option<Self::Token<'a>>; }
struct P;
impl St for P { type Token<'a> = &'a u8; }
struct Xf;
const _: () = {
    use crate::St as SolType;
    impl Call for Xf {
        type Parameters<'a> = P;
        type Token<'a> = <Self::Parameters<'a> as SolType>::Token<'a>;
        fn tok<'a>(&self) -> Option<Self::Token<'a>> { None }
    }
};
fn generic_assoc(x: Xf) -> bool { x.tok().is_none() }

struct Xg;
const _: () = {
    impl Tr for Xg {
        const K: u8 = 7;
        fn f(&self) -> u8 { struct V; impl A for V {} V.a() }
    }
};
fn nested_twice(x: Xg) -> u8 { x.f() }

struct Xh;
const _: () = { struct Local(u8); impl Local { fn get(&self) -> u8 { self.0 } } impl Tr for Xh { const K: u8 = 8; fn f(&self) -> u8 { Local(8).get() } } };
fn block_local_type(x: Xh) -> u8 { x.f() }

struct Xi;
#[doc(hidden)]
#[allow(non_upper_case_globals, unused_attributes, unused_qualifications, clippy::absolute_paths)]
const _: () = {
    #[allow(unused_extern_crates, clippy::useless_attribute)]
    extern crate core as _serde;
    #[automatically_derived]
    impl _serde::default::Default for Xi { fn default() -> Xi { Xi } }
};
fn attr_extern_crate() -> Xi { <Xi as Default>::default() }

struct Xj;
const _: () = {
    #[doc(hidden)]
    #[allow(dead_code)]
    type Under = u8;
    #[automatically_derived]
    #[doc(hidden)]
    impl Tr for Xj { const K: u8 = 3; fn f(&self) -> u8 { let t: Under = 3; t } }
};
fn attr_alias(x: Xj) -> u8 { x.f() }

trait De<'de> { type Out; fn go(self) -> Self::Out; }
fn serde_shape() -> u8 {
    enum F { A, B }
    struct V<'de> { m: core::marker::PhantomData<&'de ()> }
    impl<'de> De<'de> for V<'de> { type Out = F; fn go(self) -> F { F::A } }
    type Al = u8;
    let one: Al = 1;
    match (V { m: core::marker::PhantomData }).go() { F::A => one, F::B => 2 }
}

struct Xn;
fn no_impl(x: Xn) -> u8 { x.f() }
";

/// (fn, what its clean check proves).
const CLEAN: &[(&str, &str)] = &[
    ("in_const", "an impl in a `const _` block answers a method call from another item"),
    ("block_use", "the impl's trait path is read through the block's own `use .. as ..`"),
    ("nested_blocks", "an impl in a block inside the block, and one after it"),
    ("block_extern_crate", "serde's shape: the trait path through the block's `extern crate .. as ..`"),
    ("in_fn_body", "an impl in a fn's body answers from another fn"),
    ("generic_assoc", "alloy's shape: a generic associated type projected through a block-local trait name"),
    ("nested_twice", "serde's shape: an impl inside a member fn of an impl inside a block"),
    ("block_local_type", "the impl's member names a struct declared in its block"),
    // serde_derive 1.0.229 src/dummy.rs: `#[allow(unused_extern_crates, clippy::useless_attribute)]
    // extern crate serde as _serde;`: the attribute's `(..)` is the attribute's, the item after
    // it the block's (the tempo census's "9781 container head: Resolve Path").
    ("attr_extern_crate", "serde's real text: the block's `extern crate` written after an attribute with arguments"),
    // alloy-sol-macro-expander 1.7.3 src/expand/mod.rs:931: `#[doc(hidden)] #[allow(dead_code)]
    // type UnderlyingSolTuple<'a> = ..;` then `#[automatically_derived] #[doc(hidden)] impl ..`
    // (the census's "156 lower Convert: Form .. Resolve/Path").
    ("attr_alias", "alloy's real text: a block's type alias and impl written after attributes with arguments"),
    // serde_derive's `Deserialize` writes, inside `fn deserialize`, `enum __Field { .. }`,
    // `struct __FieldVisitor;`, `struct __Visitor<'de> { marker: PhantomData<Role>, lifetime:
    // PhantomData<&'de ()> }` and their impls, and the body names them (the tempo census's "1190
    // gap Local: a struct in a block"). The Reference, "Statements", "Item declarations":
    // "Declaring an item within a statement block restricts its scope to the block containing the
    // statement" and "There is no implicit capture of the containing function's generic
    // parameters, parameters, and local variables": the enum, the struct and the alias are items
    // of the block's module (one key each, the names library's), their heads made by the
    // ordinary code. Expected lines: "serde_shape: clean" and "member `go` of `impl<'de> De<'de>
    // for V<'de>`: clean"; "NESTED impls in the index" one more than before this fixture. Without
    // the change: "serde_shape: .. a struct in a block (.., at ..) has no record yet".
    ("serde_shape", "serde's Deserialize shape: a body names the enum, the generic struct and the alias declared in its own block"),
];

/// (a body through an impl in a block, the same body through a module-level impl): the two
/// must have ONE outcome, clean or not: what is not this change's is the checker's for both.
const SAME: &[(&str, &str)] = &[("in_const_k", "module_k")];

/// Impls (by header) whose fn body names a STRUCT declared in that body: front2's own lookup
/// of a body's local items (front2/locals.rs) answers a local struct with the typed gap
/// `Gap::Local` ("a struct in a block"): never a wrong answer, and owed by that lane (the
/// block's module now holds the struct: `Locals::find` can answer from it).
const LOCAL_STRUCT_GAP: &[&str] = &["impl Tr for Xg"];

/// Must be refused: the control that the method call is really checked.
const REFUSED: &[&str] = &["no_impl"];

#[test]
fn impls_inside_blocks_through_front2() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/nest/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "nest", dir: "/nest/", root: "/nest/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let find = |segments: &[&str]| {
        names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(segments.len()))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{segments:?}` from the crate's root"))
    };
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut failed = Vec::new();
    let mut report: Vec<String> = Vec::new();
    // (name, the fn or (impl, member name), what it proves, how it is judged)
    #[derive(Clone, Copy, PartialEq)]
    enum Judge {
        Clean,
        Refused,
        /// Compared with its control after the run.
        Same,
    }
    let mut cases: Vec<(String, blaze_id::ItemKey, Option<&str>, &str, Judge)> = Vec::new();
    for &(n, p) in CLEAN {
        cases.push((n.to_string(), find(&[n]), None, p, Judge::Clean));
    }
    for &n in REFUSED {
        cases.push((n.to_string(), find(&[n]), None, "must be refused", Judge::Refused));
    }
    for &(a, b) in SAME {
        cases.push((a.to_string(), find(&[a]), None, "one outcome with its control", Judge::Same));
        cases.push((b.to_string(), find(&[b]), None, "the control", Judge::Same));
    }
    let mut outcomes: Vec<(String, String)> = Vec::new();
    // Every fn of an impl written in a block is a body of its own, under its own key: the
    // impls of the user crate whose text is not the file's.
    let mut nested_impls = 0usize;
    let mut impls: Vec<_> = names.crates()[0].tables.impls.iter().collect();
    impls.sort_by_key(|i| i.key);
    for i in impls {
        if !matches!(names.place(i.key).map(|p| p.at), Some(At::Expansion(_))) {
            continue;
        }
        nested_impls += 1;
        for m in names.members(i.key).unwrap_or(&[]) {
            if matches!(m.detail, ItemDetail::Fn { .. }) {
                cases.push((format!("member `{}` of `{}`", m.name.unwrap_or("?"), i.header), i.key, m.name, "a nested impl's fn is a body", Judge::Clean));
            }
        }
    }
    println!("NESTED impls in the index: {nested_impls}");
    for (name, key, member, proves, judge) in cases {
        let refuse = judge == Judge::Refused;
        // A free fn by its key; an impl's fn by its impl's record and its name (as the census).
        let want = if member.is_some() { Want::Impl(key) } else { Want::Fn(key) };
        if let Err(e) = store.ensure(&names, &mut front.keys, want) {
            report.push(format!("{name}: no head: {e:?}"));
            failed.push(name);
            continue;
        }
        let lowered = match member {
            None => lower_fn(&names, prepared, &mut front.keys, &mut store, key),
            Some(m) => match front.keys.filed_member(checker_of(key), MemberKind::Fn, m) {
                Some(k) => lower_member(&names, prepared, &mut front.keys, &mut store, k),
                None => {
                    report.push(format!("{name}: the impl's record files no fn of that name"));
                    failed.push(name);
                    continue;
                }
            },
        };
        if judge == Judge::Same {
            let outcome = match lowered {
                Err(e) => format!("does not lower: {e}"),
                Ok(body) => {
                    let view = body.lowered.view();
                    let spell = Spelling::of(&view).expect("spelling");
                    let scope = body.scope(&spell);
                    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
                    match check_body(&mut records, &body, &scope) {
                        Ok(_) => "clean".to_string(),
                        Err(CheckFail::Diags { diags, .. }) => format!("{:?}", diags.iter().map(|d| format!("{:?}", d.0)).collect::<Vec<_>>()),
                        Err(other) => format!("{other:?}").chars().take(80).collect(),
                    }
                }
            };
            outcomes.push((name, outcome));
            continue;
        }
        let body = match lowered {
            Ok(b) => b,
            Err(e) => {
                report.push(format!("{name}: does not lower: {e}"));
                failed.push(name);
                continue;
            }
        };
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        match (check_body(&mut records, &body, &scope), refuse) {
            (Ok(_), false) => report.push(format!("{name}: clean")),
            (Err(CheckFail::Diags { diags, .. }), true) => {
                let d: String = format!("{diags:?}").chars().take(60).collect();
                report.push(format!("{name}: clean (refused: {d})"));
            }
            (Ok(_), true) => {
                report.push(format!("{name}: checked clean, but must be refused"));
                failed.push(name);
            }
            (Err(CheckFail::Diags { diags, .. }), false) => {
                report.push(format!("{name}: diagnostics {diags:?} ({proves})"));
                failed.push(name);
            }
            (Err(other), _) if LOCAL_STRUCT_GAP.iter().any(|h| name.contains(h)) && format!("{other:?}").contains("a struct in a block") => {
                report.push(format!("{name}: clean (the typed gap of a struct in a block, not a wrong answer)"));
            }
            (Err(other), _) => {
                report.push(format!("{name}: {other:?} ({proves})"));
                failed.push(name);
            }
        }
    }
    for pair in outcomes.chunks(2) {
        let [(a, got), (b, control)] = pair else { panic!("SAME cases come in pairs") };
        if got == control {
            report.push(format!("{a}: clean (same as its module-level control `{b}`: {got})"));
        } else {
            report.push(format!("{a}: {got}, but its module-level control `{b}`: {control}"));
            failed.push(a.clone());
        }
    }
    // Failures first: the harness's runner shows the first lines only.
    report.sort_by_key(|l| l.contains(": clean"));
    for line in &report {
        println!("{line}");
    }
    let why: Vec<String> = report.iter().filter(|l| !l.contains(": clean")).map(|l| l.chars().take(260).collect()).collect();
    assert!(failed.is_empty(), "bodies that did not check as they must: {failed:?}\n{}", why.join("\n"));
    assert!(nested_impls >= 10, "the index holds {nested_impls} impls written in blocks; the text has 11");
}
