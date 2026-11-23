//! A type alias used where the front end had no form for it, on front2 with the real core: the
//! two faces tempo's census shows (tempo-front-9: "47 gap TypeAlias" and "21 gap TypeAlias",
//! "the type alias .. named in a body path aliases a type with fixed arguments; the checker's
//! `Def` has no form for it", in code alloy's `sol!` writes; and the impl a051fdad of
//! fixed_map, "the type alias `Iter<'a, K, V>` is refused: Program(\"a trait written with 1
//! type arguments; it takes 0\")"), reduced.
//!
//! (a) The Reference, "Type aliases": "A type alias defines a new name for an existing type".
//! So a path through `type Call<P, C> = Raw<P, C, u8>;` (`Call::new(..)`,
//! `Call { .. }`) means what it does with `Raw<P, C, u8>` written: the alias's own parameters
//! substituted, the fixed `u8` kept. (The same section: "A type alias to a tuple-struct or
//! unit-struct cannot be used to qualify that type's constructor", so `Call(..)` is not a
//! case.)
//!
//! (b) The Reference, "Associated Items", "Associated Types": "Associated types may include
//! generic parameters and where clauses; these are often referred to as generic associated
//! types, or GATs." In `<<K as Key>::MapStorage<V> as Storage<K, V>>::Iter<'a>` the `<V>` is
//! on the associated type `MapStorage`'s own segment: it is `MapStorage`'s argument, and
//! `Key` is written with none.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_panics.rs.

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

const PATHS: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
struct Raw<P, C, N> { p: P, c: C, n: N }
impl<P, C, N> Raw<P, C, N> {
    fn new(p: P, c: C, n: N) -> Self { Raw { p: p, c: c, n: n } }
}
type Call<P, C> = Raw<P, C, u8>;
fn a1() -> Raw<bool, char, u8> { Call::new(true, 'x', 1) }
fn a2() -> Raw<bool, char, u8> { Call { p: true, c: 'x', n: 1 } }
fn a3() -> Raw<bool, char, u8> { Call::<bool, char>::new(true, 'x', 1) }
fn a4(r: Call<bool, char>) -> bool { match r { Call { p, .. } => p } }
fn bad1() -> Raw<bool, char, u16> { Call::new(true, 'x', 1) }
fn bad2() -> Raw<bool, char, u16> { Call { p: true, c: 'x', n: 1 } }
struct Wrap<T> { t: T }
struct Poly<D> { alive: usize, data: D }
impl<D> Poly<D> {
    fn new(d: D) -> Self { Poly { alive: 0, data: d } }
}
type Inner<T, const N: usize> = Poly<[Wrap<T>; N]>;
fn a5() -> Poly<[Wrap<bool>; 2]> { Inner::new([Wrap { t: true }, Wrap { t: false }]) }
fn a6() -> Poly<[Wrap<bool>; 2]> { Inner { alive: 0, data: [Wrap { t: true }, Wrap { t: false }] } }
fn bad3() -> Poly<[Wrap<u16>; 2]> { Inner::new([Wrap { t: true }, Wrap { t: false }]) }
";

/// The bodies that must check clean, each with what it proves. `P` and `C` are different
/// types in each: the two parameters left to inference are two variables, not one.
const CLEAN: [(&str, &str); 6] = [
    ("a1", "`Call::new(..)`: an associated fn through the alias, no arguments written"),
    ("a2", "`Call { .. }`: a struct expression through the alias"),
    ("a3", "`Call::<bool, char>::new(..)`: the alias's own arguments written"),
    ("a4", "`Call { p, .. }`: a struct pattern through the alias"),
    // core's `type InnerSized<T, const N: usize> = PolymorphicIter<[MaybeUninit<T>; N]>;`
    // (library/core/src/array/iter.rs), reduced: one argument holds a type parameter and a
    // const parameter as the array's length. Each becomes its own hole.
    ("a5", "`Inner::new(..)`: two of the alias's parameters in one argument, `[Wrap<T>; N]`"),
    ("a6", "`Inner { .. }`: a struct expression through that alias"),
];

/// The bodies that must be a type mismatch (E0308): the alias fixes `Raw`'s third argument at
/// `u8`, and each body returns a `Raw<_, _, u16>`. A fix that drops the fixed argument (a
/// fresh variable there) passes them, the literal `1` taking `u16`.
const MISMATCH: [(&str, &str); 3] = [
    ("bad1", "`Call::new(true, 'x', 1)` is a `Raw<bool, char, u8>`, never a `Raw<_, _, u16>`"),
    ("bad2", "`Call { .. }` is a `Raw<bool, char, u8>`, never a `Raw<_, _, u16>`"),
    ("bad3", "`Inner::new([Wrap { t: true }, ..])` is a `Poly<[Wrap<bool>; 2]>`, never one of `Wrap<u16>`"),
];

/// Expected line: `test alias_path_keeps_the_fixed_argument ... ok`. Before the expansion
/// every body here was `CheckFail::Gap` with `Gap::TypeAlias` ("the type alias .. named in a
/// body path aliases a type with fixed arguments"). An alias the conversion does not expand is
/// now `Def::Alias`, which the checker expands at the segment's arguments, so each body is
/// still checked against the aliased type: an expansion that dropped the fixed argument (a
/// looser `Raw<_, _, _>`) fails on `bad1`, one that lost `[Wrap<T>; N]`'s element on `bad3`.
#[test]
fn alias_path_keeps_the_fixed_argument() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/als/src/main.rs".to_string(), PATHS.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "als", dir: "/als/", root: "/als/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: PATHS }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut failed: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut bodies = Vec::new();
    for (name, proves, clean) in CLEAN.iter().map(|&(n, p)| (n, p, true)).chain(MISMATCH.iter().map(|&(n, p)| (n, p, false))) {
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
            Ok(b) => bodies.push((name, b, proves, clean)),
            Err(e) => {
                notes.push(format!("{name}: does not lower: {e}"));
                failed.push(name.to_string());
            }
        }
    }
    for (name, body, proves, clean) in &bodies {
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        match (check_body(&mut records, body, &scope), *clean) {
            (Ok(done), true) if done.diags.is_empty() => notes.push(format!("{name}: clean")),
            (Ok(done), _) => {
                notes.push(format!("{name}: Ok with {} diagnostics; wanted {} ({proves})", done.diags.len(), if *clean { "none" } else { "a mismatch" }));
                failed.push(name.to_string());
            }
            // The mismatch is the program's own error, by the checker's code for E0308.
            (Err(CheckFail::Diags { diags, .. }), false) if format!("{diags:?}").contains("Mismatch") => notes.push(format!("{name}: a mismatch, as it must be")),
            (Err(CheckFail::Diags { diags, .. }), _) => {
                notes.push(format!("{name}: diagnostics {diags:?} ({proves})"));
                failed.push(name.to_string());
            }
            (Err(other), _) => {
                notes.push(format!("{name}: {other:?} ({proves})"));
                failed.push(name.to_string());
            }
        }
    }
    assert_eq!(bodies.len(), CLEAN.len() + MISMATCH.len(), "every body was lowered\n{}", notes.join("\n"));
    assert!(failed.is_empty(), "bodies through the alias did not check as they must: {failed:?}\n{}", notes.join("\n"));
}

/// fixed_map's shape (its src/map.rs: `type Iter<'a, K, V> = <<K as Key>::MapStorage<V> as
/// MapStorage<K, V>>::Iter<'a>;` used as `type IntoIter = Iter<'a, K, V>;` in `impl<'a, K, V>
/// IntoIterator for &'a Map<K, V> where K: Key`), with the crate's own traits in place of
/// core's. `Bad` writes two arguments on an associated type that declares one.
const GAT: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
trait Storage<K, V> { type Iter<'a> where Self: 'a; }
trait Key: Sized { type MapStorage<V>; }
struct Map<K, V> { k: K, v: V }
struct Other<K, V> { k: K, v: V }
type Iter<'a, K, V> = <<K as Key>::MapStorage<V> as Storage<K, V>>::Iter<'a>;
type Bad<'a, K, V> = <<K as Key>::MapStorage<V, V> as Storage<K, V>>::Iter<'a>;
trait IntoIt { type IntoIter; }
impl<'a, K, V> IntoIt for &'a Map<K, V> where K: Key {
    type IntoIter = Iter<'a, K, V>;
}
impl<'a, K, V> IntoIt for &'a Other<K, V> where K: Key {
    type IntoIter = Bad<'a, K, V>;
}
";

/// Expected line: `test alias_of_a_generic_associated_type_builds_its_impl ... ok`. Without
/// the fix the impl for `&'a Map<K, V>` is refused ("the type alias `Iter<'a, K, V>` is
/// refused: Program(\"a trait written with 1 type arguments; it takes 0\")"); the impl for
/// `&'a Other<K, V>` must stay refused, naming the count ("it takes 1").
#[test]
fn alias_of_a_generic_associated_type_builds_its_impl() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/gat/src/main.rs".to_string(), GAT.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "gat", dir: "/gat/", root: "/gat/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: GAT }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let mut built = Vec::new();
    let mut refused = Vec::new();
    for (k, _, _) in names.items(0) {
        let Some(header) = names.item_text(k).and_then(|t| t.impl_header) else { continue };
        match store.ensure(&names, &mut front.keys, Want::Impl(k)) {
            Ok(_) => built.push(header.to_string()),
            Err(e) => refused.push((header.to_string(), format!("{e:?}"))),
        }
    }
    assert!(built.iter().any(|h| h.contains("Map<K, V>")), "the impl for `&'a Map<K, V>` is built; built {built:?}, refused {refused:?}");
    let other = refused.iter().find(|(h, _)| h.contains("Other<K, V>")).unwrap_or_else(|| panic!("the impl for `&'a Other<K, V>` is refused; built {built:?}, refused {refused:?}"));
    assert!(other.1.contains("it takes 1"), "refused by the associated type's own count: {}", other.1);
}
