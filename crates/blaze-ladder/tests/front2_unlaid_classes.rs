//! A head whose regions are not laid is refused by a TYPED cause and the signature part it was
//! met in (front2 types.rs `NotLaidWhy`, `SigPart`; head_record.rs `Unlaid::NotLaid { why,
//! part }`), the same answer in every store (tables/head.rs form 12), so the profile counts
//! tempo's refusals by class (`[NotLaid/ProjectionBoundElsewhere/Output]`) and never by one
//! "a region position the signature's walk does not lay". One fixture per family of causes,
//! each with its TWIN that IS laid (rule 23): the twin differs from the refused head in the
//! one thing the cause names.
//!
//! Rust's rules each fixture states: the Reference, "Paths" ("Paths in types": `T::Name` is
//! `<T as Trait>::Name` for the one bound naming the trait), "Associated items", "Lifetime
//! elision", "Implementations". Needs the installed toolchain under
//! `$HOME/.blaze/v2/toolchain`, as tests/front2_region_rules.rs does.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::head_record::Unlaid;
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::{MemberKind, checker_of};
use blaze_ladder::front2::types::{NotLaidWhy, SigPart};
use blaze_ladder::refusal::SourceFile;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};
use blaze_typeck::regions::Region;

const P0: Region = Region::Param(0);

/// What one fixture asks of the crate.
#[derive(Clone, Copy, Debug)]
enum Ask {
    /// A free fn's head, by name.
    Free(&'static str),
    /// A trait's method's head: the trait's name, the method's.
    TraitFn(&'static str, &'static str),
    /// The impl header of the impl that has the method of this name.
    HeaderOf(&'static str),
    /// The column of the `type Name = ..;` value (the second) of the impl that has the method
    /// of this name (the first), filed with the impl's regions (`HeadRegions::impl_assoc`).
    AssocOf(&'static str, &'static str),
    /// The signature of the impl's member fn of this name (its own want, `Want::MemberFn`).
    ImplFn(&'static str),
    /// The type of the impl's member constant of this name (made with the impl's head).
    ImplConst(&'static str),
}

/// A head's answer: its signature's (or header's) column, or its refusal's kind as filed.
#[derive(Debug, PartialEq, Eq)]
enum Got {
    Laid(Vec<Region>),
    Refused(Option<Unlaid>),
}

fn toolchain() -> Toolchain {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"))
}

/// Lowers `text` as one crate of edition 2024 and answers each of `asks`.
fn heads(text: &'static str, asks: &[Ask]) -> Vec<Got> {
    let tc = toolchain();
    let files = [("/krate/src/main.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("a known edition");
    let user = UserCrate { name: "krate", dir: "/krate/", root: "/krate/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let g = Alloc::global();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, g).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, g);
    let impls: Vec<_> = names.items(0).map(|(k, _, _)| k).filter(|&k| names.item_text(k).is_some_and(|t| t.impl_header.is_some())).collect();
    let mut out = Vec::new();
    for &ask in asks {
        let found = |ns: Namespace, n: &str| {
            let found = names::path(&names, names.root(0), ns, Written { kind: PathKind::Plain, segments: &[n] }, g).unwrap_or_else(|e| panic!("`{n}`: {e}"));
            found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key
        };
        let (want, key) = match ask {
            Ask::Free(n) => {
                let lib = found(Namespace::Values, n);
                (Want::Fn(lib), checker_of(lib))
            }
            // A trait's members' signatures are made with its members (`Want::Members`).
            Ask::TraitFn(t, n) => {
                let lib = found(Namespace::Types, t);
                let want = Want::Members(lib);
                store.ensure(&names, &mut front.keys, want).unwrap_or_else(|e| panic!("`{t}`: no members: {e:?}"));
                let mk = front.keys.filed_member(checker_of(lib), MemberKind::Fn, n).unwrap_or_else(|| panic!("`{t}::{n}`: no such member"));
                (want, mk)
            }
            Ask::HeaderOf(n) | Ask::AssocOf(n, _) => {
                let mut hit = None;
                for &k in &impls {
                    store.ensure(&names, &mut front.keys, Want::Impl(k)).unwrap_or_else(|e| panic!("an impl: no head: {e:?}"));
                    if front.keys.filed_member(checker_of(k), MemberKind::Fn, n).is_some() {
                        hit = Some((Want::Impl(k), checker_of(k)));
                    }
                }
                hit.unwrap_or_else(|| panic!("`{n}`: in no impl"))
            }
            // An impl's constant is made with its impl's head (`Want::Impl`): its regions are
            // its type's, as a no-input fn's output.
            Ask::ImplConst(n) => {
                let mut hit = None;
                for &k in &impls {
                    store.ensure(&names, &mut front.keys, Want::Impl(k)).unwrap_or_else(|e| panic!("an impl: no head: {e:?}"));
                    if let Some(mk) = front.keys.filed_member(checker_of(k), MemberKind::Const, n) {
                        hit = Some((Want::Impl(k), mk));
                    }
                }
                hit.unwrap_or_else(|| panic!("`{n}`: in no impl"))
            }
            Ask::ImplFn(n) => {
                let mut hit = None;
                for &k in &impls {
                    store.ensure(&names, &mut front.keys, Want::Impl(k)).unwrap_or_else(|e| panic!("an impl: no head: {e:?}"));
                    if let Some(mk) = front.keys.filed_member(checker_of(k), MemberKind::Fn, n) {
                        hit = Some((Want::MemberFn(k, mk), mk));
                    }
                }
                hit.unwrap_or_else(|| panic!("`{n}`: in no impl"))
            }
        };
        store.ensure(&names, &mut front.keys, want).unwrap_or_else(|e| panic!("{ask:?}: no head: {e:?}"));
        assert!(store.fn_gaps.get(&key).is_none(), "{ask:?}: the head itself was refused: {:?}", store.fn_gaps.get(&key));
        let got = match store.regions.made(key) {
            Some(f) => {
                let hr = f.regions();
                Got::Laid(match ask {
                    Ask::HeaderOf(_) => hr.impl_header.as_ref().expect("an impl header column").0.as_slice().to_vec(),
                    Ask::AssocOf(_, a) => {
                        let name = blaze_typeck::Name::of(a);
                        let found = hr.impl_assoc.iter().find(|e| e.0 == name).unwrap_or_else(|| panic!("{ask:?}: no column for `{a}`"));
                        found.1.as_slice().to_vec()
                    }
                    _ => hr.sig.as_ref().expect("a signature column").as_slice().to_vec(),
                })
            }
            None => {
                let filed = store.regions.unlaid_of(key);
                // The making store's gap and the kind every store is told are one answer.
                let gap = store.region_gaps.get(&key).map(|g| g.unlaid());
                assert_eq!(gap, filed, "{ask:?}: the store's gap and the filed kind disagree");
                Got::Refused(filed)
            }
        };
        out.push(got);
    }
    out
}

fn refused(why: NotLaidWhy, part: SigPart) -> Got {
    Got::Refused(Some(Unlaid::NotLaid { why, part }))
}

/// The class a reader prints first (borrows.rs `BorrowRefused::RegionsUnread`), as text.
fn class(u: Unlaid) -> String {
    let mut s = String::new();
    u.class(&mut s).expect("a class is written");
    s
}

/// The projection family, the largest on tempo (alloy-sol-types: `trait SolTypeValue<T:
/// SolType> { fn stv_to_tokens(&self) -> T::Token<'_>; }`, the bound on the CONTAINER).
/// `T::Name` is `<T as Trait>::Name` for the bound naming `Trait` (the Reference, "Paths in
/// types"); its slots are the trait's lifetimes (none for `SolType`), the alias's own (`'_`:
/// `&self`'s by the output rule, "Lifetime elision in functions"), then its arguments' (`T`:
/// none). So `stv_to_tokens`'s column is `[P0, P0]`, wherever the bound is written.
///
/// Refused: the same projection through a trait that DECLARES a lifetime, bound on the
/// container (`T: Lt<'static>`): its lifetime is written on the container's bound, which the
/// member's walk does not read: `NotLaid/ProjectionBoundElsewhere/Output`.
///
/// Twin (laid, rule 23): the trait without a lifetime, bound on the container; and the same
/// trait with a lifetime bound on the fn itself (`U: Lt<'a>`), whose bound the walk reads.
#[test]
fn a_projection_through_a_container_bound_is_laid_unless_its_trait_has_a_lifetime() {
    const TEXT: &str = "
pub trait SolType { type Token<'a>; }
pub trait SolTypeValue<T: SolType> { fn stv_to_tokens(&self) -> T::Token<'_>; }
pub trait Lt<'t> { type Token; }
pub trait Holder<T: Lt<'static>> { fn held(&self) -> T::Token; }
pub fn own_bound<'a, U: Lt<'a>>(x: &'a u8) -> U::Token { let _ = x; loop {} }
fn main() {}
";
    let got = heads(TEXT, &[Ask::TraitFn("Holder", "held"), Ask::TraitFn("SolTypeValue", "stv_to_tokens"), Ask::Free("own_bound")]);
    assert_eq!(got[0], refused(NotLaidWhy::ProjectionBoundElsewhere, SigPart::Output), "`held`: a container-bound projection whose trait has a lifetime");
    let Got::Refused(Some(u)) = got[0] else { unreachable!() };
    assert_eq!(class(u), "NotLaid/ProjectionBoundElsewhere/Output", "the class printed first");
    assert_eq!(got[1], Got::Laid(vec![P0, P0]), "TWIN: alloy's `T::Token<'_>` under the container's `T: SolType` is laid");
    assert_eq!(got[2], Got::Laid(vec![P0, P0]), "TWIN: `U::Token` through the fn's own `U: Lt<'a>` takes `'a` (`x`'s slot, then the trait's)");
    println!("front2_unlaid_classes: projection: held {:?}; stv_to_tokens and own_bound laid", got[0]);
}

/// `Self::Name` of a supertrait: in a trait, `Self::Name` is `<Self as Super>::Name`; its
/// slots are `Super`'s lifetimes, then the name's own and arguments'. A supertrait with no
/// lifetime gives none, so `fn get(&self) -> Self::A` in `trait Sub: Base` is `[P0]`.
///
/// Refused: the supertrait declares a lifetime (`trait SubLt: BaseLt<'static>`), written on the
/// subtrait's header, not read by the member's walk: `NotLaid/SelfSupertrait/Output`.
///
/// Twin (laid): the supertrait without a lifetime.
#[test]
fn self_name_of_a_supertrait_is_laid_unless_the_supertrait_has_a_lifetime() {
    const TEXT: &str = "
pub trait Base { type A; }
pub trait Sub: Base { fn get(&self) -> Self::A; }
pub trait BaseLt<'t> { type A; }
pub trait SubLt: BaseLt<'static> { fn get_lt(&self) -> Self::A; }
fn main() {}
";
    let got = heads(TEXT, &[Ask::TraitFn("SubLt", "get_lt"), Ask::TraitFn("Sub", "get")]);
    assert_eq!(got[0], refused(NotLaidWhy::SelfSupertrait, SigPart::Output), "`get_lt`: `Self::A` of a supertrait with a lifetime");
    assert_eq!(got[1], Got::Laid(vec![P0]), "TWIN: `Self::A` of a supertrait with no lifetime is laid (`&self`'s slot alone)");
    println!("front2_unlaid_classes: supertrait: get_lt {:?}; get laid", got[0]);
}

/// A qualified path through a trait with a lifetime (`<Wrap<'a> as Tr2<'b>>::A`): the
/// Reference, "Qualified paths": `<T as Tr<'b>>::A` names `A` of the trait reference
/// `Tr<'b>` for `T`, so the alias's slots are the trait reference's lifetimes, then the
/// alias's own, then its arguments' (the self type's first). It was refused as a lifetime
/// on a segment before the last (`NotLaid/EarlySegmentLifetime/Input`, the trait's segment
/// read as an early one); now laid.
///
/// `get` (`'a` both places) is `[P0, P0]`; `mixed` (`<Wrap<'a> as Tr2<'b>>::A`) is `[P1, P0]`,
/// the trait's `'b` FIRST: the twin that tells the order (rule 23: the self type's slot
/// first would give `[P0, P1]`). Twin (laid before too): through a trait with no lifetime,
/// `<Wrap<'a> as Tr>::A` is `[P0, P0]` (the self type's, then the output's).
#[test]
fn a_qualified_path_lays_its_trait_lifetimes_first() {
    const TEXT: &str = "
pub struct Inner<'a>(pub &'a u8);
pub struct Wrap<'a>(pub &'a u8);
pub trait Tr { type A; }
impl<'a> Tr for Wrap<'a> { type A = Inner<'a>; }
pub trait Tr2<'t> { type A; }
impl<'a, 'b> Tr2<'b> for Wrap<'a> { type A = Inner<'a>; }
pub fn get<'a>(n: u8, x: <Wrap<'a> as Tr2<'a>>::A) -> u8 { let _ = x; n }
pub fn mixed<'a, 'b>(n: u8, x: <Wrap<'a> as Tr2<'b>>::A) -> u8 { let _ = x; n }
pub fn get_twin<'a>(x: <Wrap<'a> as Tr>::A) -> &'a u8 { x.0 }
fn main() {}
";
    let p1 = Region::Param(1);
    let got = heads(TEXT, &[Ask::Free("get"), Ask::Free("mixed"), Ask::Free("get_twin")]);
    assert_eq!(got[0], Got::Laid(vec![P0, P0]), "`get`: the trait's `'a`, then `Wrap<'a>`'s");
    assert_eq!(got[1], Got::Laid(vec![p1, P0]), "`mixed`: the trait's `'b` first, then `Wrap<'a>`'s");
    assert_eq!(got[2], Got::Laid(vec![P0, P0]), "TWIN: through a trait with no lifetime it is laid");
    println!("front2_unlaid_classes: qualified: get, mixed and get_twin laid");
}

/// An impl's header: `impl Ta for fn(&u8)` (the Reference, "Implementations": any type may be
/// the self type of a local trait's impl); the fn pointer's lifetime is bound by its own
/// `for<'a>`, which the header's group does not place: `NotLaid/BoundVariable/Header`.
///
/// Twin (laid): `impl Tb for fn(&'static u8)`, its one slot `'static`.
#[test]
fn an_impl_header_names_the_header() {
    const TEXT: &str = "
pub trait Ta { fn ma(&self); }
pub trait Tb { fn mb(&self); }
impl Ta for fn(&u8) { fn ma(&self) {} }
impl Tb for fn(&'static u8) { fn mb(&self) {} }
fn main() {}
";
    let got = heads(TEXT, &[Ask::HeaderOf("ma"), Ask::HeaderOf("mb")]);
    assert_eq!(got[0], refused(NotLaidWhy::BoundVariable, SigPart::Header), "`impl Ta for fn(&u8)`: a bound variable in the header");
    assert_eq!(got[1], Got::Laid(vec![Region::Static]), "TWIN: `fn(&'static u8)` is laid");
    println!("front2_unlaid_classes: header: ma's impl {:?}; mb's impl laid", got[0]);
}

/// `Self::Name` in an impl of a trait WITH a lifetime (serde's derived `impl<'de>
/// Visitor<'de> for __Visitor<'de> { fn visit_seq<A>(..) -> Result<Self::Value, A::Error> }`,
/// tempo's 167 `NotLaid/ImplTraitLifetimes/Output` before head record 29). The Reference,
/// "Paths", "`Self`": in an impl `Self` is the implementing type, and `Self::Value` is
/// `<SelfTy as Visitor<'de>>::Value` with the trait's lifetimes as the HEADER writes them.
/// Laid as the projection: the trait's lifetimes from the header's trait column, then `Self`'s
/// (the self type's), in the members' container group (`'de` P0, `'b` P1).
///
/// - `visit` in `impl<'de, 'b> Visitor<'de> for V<'b>`: `self` is `V<'b>` [P1], the output
///   `<V<'b> as Visitor<'de>>::Value` [P0 (`'de`), P1 (`Self`'s `'b`)]. Distinct lifetimes, so a
///   trait slot read from `Self`'s column (or the reverse) shows.
/// - `visit2` in `impl Vis2<'_> for W`: the header's elided trait lifetime is the impl's fresh
///   parameter P0 (the Reference, "Impl header lifetime elision"), the output [P0].
/// - NEGATIVE twin, still refused by name: `Self::Value` read through the SUPERTRAIT
///   `Visitor<'a>` of `impl Sub<'de> for V<'b>`, whose lifetimes the header does not write:
///   `NotLaid/SelfSupertrait/Output`.
#[test]
fn self_name_in_an_impl_of_a_trait_with_a_lifetime_reads_the_header() {
    const TEXT: &str = "
pub trait Visitor<'de> { type Value; fn visit(self) -> Self::Value; }
pub struct V<'b>(pub &'b u8);
impl<'de, 'b> Visitor<'de> for V<'b> { type Value = &'de u8; fn visit(self) -> Self::Value { loop {} } }
pub trait Vis2<'de> { type Value; fn visit2(self) -> Self::Value; }
pub struct W;
impl Vis2<'_> for W { type Value = u8; fn visit2(self) -> Self::Value { 0 } }
pub trait Sub<'a>: Visitor<'a> { fn sub(self) -> Self::Value; }
impl<'de, 'b> Sub<'de> for V<'b> { fn sub(self) -> Self::Value { loop {} } }
fn main() {}
";
    let p1 = Region::Param(1);
    let got = heads(TEXT, &[Ask::ImplFn("visit"), Ask::ImplFn("visit2"), Ask::ImplFn("sub")]);
    assert_eq!(got[0], Got::Laid(vec![p1, P0, p1]), "`visit`: `self` [`'b`], then `Self::Value` [`'de` from the header, `'b` from `Self`]");
    assert_eq!(got[1], Got::Laid(vec![P0]), "`visit2`: the header's elided trait lifetime, the impl's P0");
    assert_eq!(got[2], refused(NotLaidWhy::SelfSupertrait, SigPart::Output), "NEGATIVE: `Self::Value` of the supertrait `Visitor<'a>` is not read from `Sub`'s header");
    assert_eq!(class(Unlaid::NotLaid { why: NotLaidWhy::SelfSupertrait, part: SigPart::Output }), "NotLaid/SelfSupertrait/Output");
    println!("front2_unlaid_classes: impl trait lifetimes: visit and visit2 laid; sub {:?}", got[2]);
}

/// `Self::Name` of a trait WITH a lifetime outside a member fn's signature: in a member
/// constant's type and in a member fn's where clause (head record 31: every member walk is given
/// the impl's trait column, heads.rs `SelfGiven`; before, only the signature's walk was, and
/// these were `NotLaid/ImplTraitLifetimes`). The Reference, "Paths", "`Self`": the same
/// projection `<V<'b> as Visitor<'de>>::Value` wherever the impl's member writes it.
///
/// - `C` in `impl<'de, 'b> Visitor<'de> for V<'b>`: `Option<Self::Value>` laid as a no-input
///   fn's output, the projection [P0 (`'de` from the header), P1 (`Self`'s `'b`)].
/// - `w` with `where T: From<Self::Value>`: laid (its signature `self` [P1]; the clause's
///   column is filed beside it).
/// - NEGATIVE twins, still refused: the same through the SUPERTRAIT `Visitor<'a>` of `impl
///   Sub<'de> for V<'b>` (`D`, `sw`), whose lifetimes the header does not write.
#[test]
fn self_name_of_a_trait_with_a_lifetime_in_a_constant_and_a_where_clause() {
    const TEXT: &str = "
pub trait Visitor<'de> { type Value; const C: Option<Self::Value>; fn w<T>(self) -> u8 where T: From<Self::Value>; }
pub struct V<'b>(pub &'b u8);
impl<'de, 'b> Visitor<'de> for V<'b> {
    type Value = &'de u8;
    const C: Option<Self::Value> = None;
    fn w<T>(self) -> u8 where T: From<Self::Value> { 0 }
}
pub trait Sub<'a>: Visitor<'a> { const D: Option<Self::Value>; fn sw<T>(self) -> u8 where T: From<Self::Value>; }
impl<'de, 'b> Sub<'de> for V<'b> {
    const D: Option<Self::Value> = None;
    fn sw<T>(self) -> u8 where T: From<Self::Value> { 0 }
}
fn main() {}
";
    let p1 = Region::Param(1);
    let got = heads(TEXT, &[Ask::ImplConst("C"), Ask::ImplFn("w"), Ask::ImplConst("D"), Ask::ImplFn("sw")]);
    println!("front2_unlaid_classes: Self::Name beyond the signature: {got:?}");
    assert_eq!(got[0], Got::Laid(vec![P0, p1]), "`C`: `Option<Self::Value>`, [`'de` from the header, `'b` from `Self`]");
    assert_eq!(got[1], Got::Laid(vec![p1]), "`w`: laid with its where clause's `Self::Value`");
    assert!(matches!(got[2], Got::Refused(_)), "NEGATIVE: `D`'s `Self::Value` of the supertrait is not read from `Sub`'s header: {:?}", got[2]);
    assert!(matches!(got[3], Got::Refused(_)), "NEGATIVE: `sw`'s clause's `Self::Value` of the supertrait: {:?}", got[3]);
}

/// `Self::Name` of a trait WITH a lifetime in the impl's OWN `type X = ..;` value and in its
/// header's own where clause (head record 32: heads.rs `impl_` lays the header's columns first,
/// heads_regions.rs `impl_header_regions`, then walks the values and the clauses given its trait
/// column; before, both walks ran before or beside the header's and were given `Self`'s regions
/// alone, so these refused the whole impl's regions, `NotLaid/ImplTraitLifetimes`). The
/// Reference, "Paths", "`Self`": in the impl, `Self::Value` is `<SelfTy as Trait<'de>>::Value`
/// with the trait's lifetimes as the header writes them, its where clauses included.
///
/// - `Out` in `impl<'de, 'b> Visitor<'de> for V<'b>`: `Option<Self::Value>`, [P0 (`'de` from
///   the header), P1 (`Self`'s `'b`)].
/// - `impl<'de, 'b, T> Held<'de> for H<'b, T> where T: From<Self::Value>`: laid, its self
///   column [P1].
/// - NEGATIVE twins, refused (the cause: the header does not write the lifetimes of the trait
///   the name is read through): the same value and the same clause through the SUPERTRAIT
///   `Visitor<'a>` / `Held<'a>` of `impl Sub<'de> for V<'b>` / `impl Sub2<'de> for H<'b, T>`.
///
/// - `impl<T> Plain for P<T> where T: From<Self::Value>`: a trait with NO lifetime, laid (an
///   empty self column). Any `Self::X` in a trait impl's header clause is the same path: heads.rs
///   `impl_` lowered the where clauses BEFORE the trait reference, with `Self` an inherent
///   impl's (types.rs `SelfTy::Ty(_, None)`), so the whole impl had no head ("`Self::Value`
///   names no associated type of the impl", run t60 at 05:56); now the trait reference first.
///   The twin with the error standing: [`self_name_in_an_inherent_impls_header_clause_names_no_associated_type`].
///
/// UNSURE: whether rustc accepts `where T: From<Self::Value>` on the impl defining `Value`
/// (no use site here asks it to select the impl); the head is all this reads.
#[test]
fn self_name_in_a_trait_impls_header_clause_and_value_reads_the_impls_trait() {
    const TEXT: &str = "
pub trait Plain { type Value; fn plain(self) -> u8; }
pub struct P<T>(pub T);
impl<T> Plain for P<T> where T: From<Self::Value> {
    type Value = u8;
    fn plain(self) -> u8 { 0 }
}
pub trait Visitor<'de> { type Value; type Out; fn visit(self) -> u8; }
pub struct V<'b>(pub &'b u8);
impl<'de, 'b> Visitor<'de> for V<'b> {
    type Value = &'de u8;
    type Out = Option<Self::Value>;
    fn visit(self) -> u8 { 0 }
}
pub trait Held<'de> { type Value; fn held(self) -> u8; }
pub struct H<'b, T>(pub &'b T);
impl<'de, 'b, T> Held<'de> for H<'b, T> where T: From<Self::Value> {
    type Value = &'de u8;
    fn held(self) -> u8 { 0 }
}
pub trait Sub<'a>: Visitor<'a> { type Out2; fn sub(self) -> u8; }
impl<'de, 'b> Sub<'de> for V<'b> {
    type Out2 = Option<Self::Value>;
    fn sub(self) -> u8 { 0 }
}
pub trait Sub2<'a>: Held<'a> { fn sub2(self) -> u8; }
impl<'de, 'b, T> Sub2<'de> for H<'b, T> where T: From<Self::Value> {
    fn sub2(self) -> u8 { 0 }
}
fn main() {}
";
    let p1 = Region::Param(1);
    let got = heads(TEXT, &[Ask::AssocOf("visit", "Out"), Ask::HeaderOf("held"), Ask::AssocOf("sub", "Out2"), Ask::HeaderOf("sub2"), Ask::HeaderOf("plain")]);
    println!("front2_unlaid_classes: Self::Name in an impl's value and header clause: {got:?}");
    assert_eq!(got[4], Got::Laid(vec![]), "`Plain`'s impl: a trait with no lifetime, its header clause's `Self::Value` read through it");
    assert_eq!(got[0], Got::Laid(vec![P0, p1]), "`Out`: `Option<Self::Value>`, [`'de` from the header, `'b` from `Self`]");
    assert_eq!(got[1], Got::Laid(vec![p1]), "`Held`'s impl: laid with its where clause's `Self::Value`");
    assert!(matches!(got[2], Got::Refused(_)), "NEGATIVE: `Out2`'s `Self::Value` of the supertrait is not read from `Sub`'s header: {:?}", got[2]);
    assert!(matches!(got[3], Got::Refused(_)), "NEGATIVE: the header clause's `Self::Value` of the supertrait: {:?}", got[3]);
}

/// The twin of [`self_name_in_a_trait_impls_header_clause_and_value_reads_the_impls_trait`],
/// with the error standing: an INHERENT impl has no trait reference, so `Self::Value` in its
/// header clause names no associated type, even though `Q<T>` implements a trait declaring
/// `Value` (rustc: E0223, "ambiguous associated type", written `<Q<T> as Plain>::Value`;
/// inherent associated types are unstable). The impl has no head, named by that error; the trait
/// impl beside it has one.
#[test]
fn self_name_in_an_inherent_impls_header_clause_names_no_associated_type() {
    const TEXT: &str = "
pub trait Plain { type Value; fn plain(self) -> u8; }
pub struct Q<T>(pub T);
impl<T> Plain for Q<T> {
    type Value = u8;
    fn plain(self) -> u8 { 0 }
}
impl<T> Q<T> where T: From<Self::Value> {
    pub fn inherent(self) -> u8 { 0 }
}
fn main() {}
";
    let tc = toolchain();
    let files = [("/krate/src/main.rs".to_string(), TEXT.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("a known edition");
    let user = UserCrate { name: "krate", dir: "/krate/", root: "/krate/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let g = Alloc::global();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: TEXT }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, g).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, g);
    let impls: Vec<_> = names.items(0).map(|(k, _, _)| k).filter(|&k| names.item_text(k).is_some_and(|t| t.impl_header.is_some())).collect();
    assert_eq!(impls.len(), 2, "the trait impl and the inherent impl");
    let (mut trait_impl, mut inherent) = (None, None);
    for &k in &impls {
        let text = names.item_text(k).map(|t| t.text).unwrap_or("");
        let got = store.ensure(&names, &mut front.keys, Want::Impl(k)).map_err(|e| format!("{e:?}"));
        if text.contains("Plain for") { trait_impl = Some(got) } else { inherent = Some(got) }
    }
    let trait_impl = trait_impl.expect("the trait impl was asked");
    assert!(trait_impl.is_ok(), "the trait impl has a head: {trait_impl:?}");
    let inherent = inherent.expect("the inherent impl was asked");
    let Err(why) = inherent else { panic!("NEGATIVE: the inherent impl's `Self::Value` was read through a trait") };
    assert!(why.contains("names no associated type of the impl"), "the refusal names the cause: {why}");
}
