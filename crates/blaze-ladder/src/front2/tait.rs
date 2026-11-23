//! A type alias whose type writes `impl Trait`: TYPE ALIAS IMPL TRAIT (RFC 2515, unstable
//! `type_alias_impl_trait`, as the pinned nightly implements it with `#[define_opaque]`).
//!
//! The case (tempo census, 34 heads and about 90 bodies): the pinned std writes `type
//! LazyResolve = impl (FnOnce() -> Capture) + Send + Sync + UnwindSafe;` (library/std/src/
//! backtrace.rs) and `struct LazyLock<Capture, LazyResolve>` in `Inner`; the alias's head was
//! refused "an `impl Trait` type outside a fn's parameters and return type" (types.rs, the
//! `Impl` arm: no `impl` of the alias was in `Env::anon`), and with it every head and body that
//! reads `std::backtrace::Backtrace`.
//!
//! THE RULE. Each `impl Trait` written in the alias's type is ONE opaque type of the alias (the
//! RFC: "a type alias ... `type Foo = impl Bar;` ... `Foo` is an opaque type"), numbered as a
//! fn's return opaques are (super::types `return_impls`: preorder, left to right), keyed by the
//! alias's key and that number (super::keys `opaque_key`), its identity the alias's generic
//! parameters (`type A<T> = impl Tr<T>;`: `A<u8>` and `A<u16>` are two types). Its bounds are
//! the written bounds with the opaque as `Self`, and its implied `Sized` (preds.rs
//! `impl_bounds`, the one home for an opaque's bounds). It is the SAME representation as a
//! fn's return-position `impl Trait` (`PNode::Opaque(key, identity)` with its bounds as the
//! opaque key's predicates, `Store::opaques` naming its owner: rule 30); only its owner is an
//! alias, which [`ALIAS_OPAQUE`] says in the owner row's index.
//!
//! Its HIDDEN type is fixed by its defining uses: the fns written `#[define_opaque(Alias)]`.
//! Everywhere else it is opaque: a body sees only its bounds (a method through a bound
//! resolves), and an AUTO TRAIT of it leaks from the hidden type (the Reference, "Impl trait",
//! "Auto trait leakage"). That leak needs the defining fns' checks, which this front end does
//! not make for an alias yet (the checker's opaque definition, blaze-typeck rules/opaque_def.rs,
//! knows a fn's own opaques only): `Send`, `Sync` or any auto trait asked of an alias's opaque
//! is refused BY NAME ([`HIDDEN_OWED`], front2/opaque_auto.rs `run`), never answered by a guess.

use std::vec::Vec;

use blaze_id::ItemKey as LibKey;
use blaze_lower::{View, form as lf};
use blaze_own::Alloc;
use blaze_surface::Range;
use blaze_typeck::{ItemKey, List, PNode, PTy, PortableTys};

use super::heads_regions::Scope;
use super::keys::opaque_key;
use super::preds::Preds;
use super::source::NamesSource;
use super::types::{Builder, Env, Heads, Why, intern, intern_list, return_impls};

/// The bit an opaque's owner row (`Store::opaques`: owner key, index) sets in its index when
/// the owner is a type alias, not a fn: the `j`th `impl Trait` of alias `a` is `(checker_of(a),
/// ALIAS_OPAQUE | j)`. A fn has far fewer than 2^31 return opaques, so no fn's index has it.
pub const ALIAS_OPAQUE: u32 = 1 << 31;

/// Why an auto trait of an alias's opaque is not answered: its hidden type is its defining
/// fns', which are not checked for it here (module doc). The words front2/opaque_auto.rs's
/// refusal carries.
pub const HIDDEN_OWED: &str = "an auto trait of a type alias's `impl Trait` (type_alias_impl_trait): its hidden type is fixed by the fns written `#[define_opaque(..)]` with the alias, which this front end does not check for it yet";

/// An alias whose type writes [`ALIAS_OPAQUE`] or more `impl`s (rule 18: the numbering's cap).
const TOO_MANY: &str = "a type alias with more than 2^31 `impl Trait` types";

/// Whether an opaque owner row's `index` is an alias's ([`ALIAS_OPAQUE`]).
pub fn of_alias(index: u32) -> bool {
    index & ALIAS_OPAQUE != 0
}

/// The opaques an alias's written type `t` (in `v`) has, each with the type id it is written
/// at and the opaque type it is: `Opaque(opaque_key(alias, j), identity)`, `identity` the
/// alias's own generic parameters (`generics` of them, the const ones `consts` by index, as
/// heads.rs `own_generics` lists them). Empty for an alias that writes no `impl`. The list is
/// the `Env::anon` the alias's type is built with, so each `impl` is its opaque.
pub(super) fn alias_opaques<'a>(tys: &mut PortableTys<'_>, v: &View<'_>, t: lf::TypeId, alias: ItemKey, generics: u32, consts: &[(u32, PTy)], alloc: Alloc<'a>) -> Result<Vec<(lf::TypeId, PTy), Alloc<'a>>, Why> {
    let mut written: Vec<lf::TypeId, Alloc<'a>> = Vec::new_in(alloc);
    // A type macro in the alias's type is expanded by its own reader (pattern_ty.rs
    // `macro_ty`), which refuses an `impl` its expansion writes by name: only the `impl`s
    // written in the alias's own text are its opaques here.
    return_impls(v, t, &mut written)?;
    let mut out: Vec<(lf::TypeId, PTy), Alloc<'a>> = Vec::with_capacity_in(written.len(), alloc);
    if written.is_empty() {
        return Ok(out);
    }
    let identity = identity(tys, generics, consts, alloc)?;
    for (j, &at) in written.iter().enumerate() {
        let j = u32::try_from(j).ok().filter(|&j| j < ALIAS_OPAQUE).ok_or(Why::Unsupported(TOO_MANY, Range::new(0, 0)))?;
        out.push((at, intern(tys, PNode::Opaque(opaque_key(alias, j), identity))?));
    }
    Ok(out)
}

/// `Param(0)..Param(generics)`, a const parameter's as `ConstParam`: the alias's parameters
/// as its opaques' identity (every parameter in scope is captured, as a fn's return opaque
/// captures its fn's: heads.rs `fn_head`'s `ident`).
fn identity(tys: &mut PortableTys<'_>, generics: u32, consts: &[(u32, PTy)], alloc: Alloc<'_>) -> Result<List, Why> {
    let mut l: Vec<PTy, Alloc<'_>> = Vec::with_capacity_in(generics as usize, alloc);
    for i in 0..generics {
        let node = if consts.iter().any(|&(c, _)| c == i) { PNode::ConstParam(i) } else { PNode::Param(i) };
        l.push(intern(tys, node)?);
    }
    intern_list(tys, &l)
}

/// One opaque of an alias, ready to file: its key, its owner row's index (with
/// [`ALIAS_OPAQUE`]) and its predicates.
pub(super) struct AliasOpaque {
    pub key: ItemKey,
    pub index: u32,
    pub preds: Preds,
}

/// The bounds of each of `opaques` ([`alias_opaques`]) of alias `alias`, read in the alias's
/// `env` (whose `anon` is `opaques`, so a bound naming another of the alias's `impl`s reads it
/// as that opaque), with `sized` the program's `Sized` (the implied bound). The one reading of
/// an opaque's bounds (preds.rs `impl_bounds`), as a fn's return opaques' are read. `scope`
/// names the alias's lifetimes (heads_regions.rs `Scope::item` over the alias's text).
#[expect(clippy::too_many_arguments, reason = "the alias's parts, each read once by the caller")]
pub(super) fn alias_opaque_preds<'n, 'a, H: Heads, S: NamesSource<'n>>(
    b: &Builder<'_, H, S>,
    tys: &mut PortableTys<'_>,
    env: &Env<'_>,
    v: &View<'_>,
    alias: ItemKey,
    opaques: &[(lf::TypeId, PTy)],
    sized: Option<LibKey>,
    scope: &Scope,
    alloc: Alloc<'a>,
) -> Result<Vec<AliasOpaque, Alloc<'a>>, Why> {
    let mut out: Vec<AliasOpaque, Alloc<'a>> = Vec::with_capacity_in(opaques.len(), alloc);
    for (j, &entry) in opaques.iter().enumerate() {
        // `alias_opaques` numbered them below `ALIAS_OPAQUE`.
        let j = j as u32;
        let mut preds = Preds::new_in(scope.clone_in(scope.alloc()));
        b.impl_bounds(tys, env, v, &[entry], sized, &mut preds)?;
        out.push(AliasOpaque { key: opaque_key(alias, j), index: ALIAS_OPAQUE | j, preds });
    }
    Ok(out)
}

/// The most segments a `#[define_opaque]` path is read with ([`names_alias`]).
const MOST_SEGMENTS: usize = 32;

/// Why [`defining_fn`] found no fn that defines an alias (rule 19).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefinerMiss {
    /// A front end that does not search for defining fns (the seam's default).
    NotAnswered,
    /// The alias's checker key has no library key (it is no item the names hold).
    NoLibraryKey,
    /// The names source holds no crate for the alias.
    NoCrate,
    /// The alias's crate's items were not read (`NamesSource::each_item` said `false`).
    NotRead,
    /// No fn of the alias's crate writes `#[define_opaque(..)]` with a path naming it. rustc
    /// reports this as "unconstrained opaque type" (rustc_hir_analysis collect/type_of/opaque.rs
    /// `find_opaque_ty_constraints_for_tait`). A fn declared in a body, or a member of an impl
    /// or trait, is not searched yet (it is no row of `each_item`).
    NoDefiner,
}

impl DefinerMiss {
    /// The words of the refusal.
    pub fn form(self) -> &'static str {
        match self {
            DefinerMiss::NotAnswered => "this front end does not find a type alias's defining fn",
            DefinerMiss::NoLibraryKey => "the type alias has no library key: it is no item the names hold",
            DefinerMiss::NoCrate => "the names source holds no crate for the type alias",
            DefinerMiss::NotRead => "the type alias's crate's items were not read",
            DefinerMiss::NoDefiner => "no fn of the type alias's crate writes `#[define_opaque(..)]` naming it (a fn in a body or an impl is not searched yet)",
        }
    }
}

/// The fn that defines the `impl Trait` types of type alias `alias`: the first fn of its crate,
/// in key order, whose `#[define_opaque(..)]` lists a path that resolves (in the type namespace,
/// at the fn's module) to the alias. rustc searches the WHOLE crate for an alias's defining uses
/// (`find_opaque_ty_constraints_for_tait`: `hir_walk_toplevel_module`), and makes every defining
/// fn give the same hidden type (an error otherwise), so any one of them answers. The hidden
/// type is in that fn's check record: its written output names the alias, which the checker
/// opens as its own (blaze-typeck rules/opaque_def.rs, machine.rs `PNode::Opaque` when
/// `opening`). std's `type LazyResolve = impl ..` (backtrace.rs:433) is defined by
/// `#[define_opaque(LazyResolve)] fn lazy_resolve` (:435). Before (tempo t1935d, 10 lines: eyre's
/// `DefaultHandler::default_with`), the alias itself was taken for the fn to check: "`LazyResolve`'s
/// body did not lower: .. NotAHead(.., "a type alias")".
///
/// INTERIM (D197, audit F14): replaced by the items record's `define_opaque` column: delete
/// this crate-wide text read then. It reads the text of every fn of the alias's crate, ONCE per
/// crate per command ([`Definers`] keeps the crate's answer): N aliases of one crate cost one
/// read of its fns, and the memo holds one row per `#[define_opaque]` path that names a type.
/// It runs only when a layout meets an alias's opaque.
pub fn defining_fn<'n, S: NamesSource<'n>>(definers: &Definers<'_>, names: S, alias: LibKey) -> Result<LibKey, DefinerMiss> {
    let krate = names.crate_of(alias).ok_or(DefinerMiss::NoCrate)?;
    let mut crates = definers.crates.borrow_mut();
    let at = match crates.iter().position(|row| row.0 == krate) {
        Some(at) => at,
        None => {
            crates.push((krate, crate_definers(names, krate, definers.alloc)));
            crates.len() - 1
        }
    };
    let rows = crates[at].1.as_ref().map_err(|miss| *miss)?;
    rows.iter().find(|row| row.0 == alias).map(|row| row.1).ok_or(DefinerMiss::NoDefiner)
}

/// Each crate's `(type, the first fn whose #[define_opaque] names it)` rows, made the first time
/// [`defining_fn`] asks about one of the crate's aliases (or why the crate could not be read).
/// One per command, held by the program (program.rs `Front2Program`, lazy `LazyProgram`).
pub struct Definers<'a> {
    crates: core::cell::RefCell<Vec<(usize, Result<Vec<(LibKey, LibKey), Alloc<'a>>, DefinerMiss>), Alloc<'a>>>,
    alloc: Alloc<'a>,
}

impl<'a> Definers<'a> {
    /// No crate read yet.
    pub fn new_in(alloc: Alloc<'a>) -> Definers<'a> {
        Definers { crates: core::cell::RefCell::new(Vec::new_in(alloc)), alloc }
    }
}

/// Every fn of crate `krate` (in key order, `each_item`) with each type its `#[define_opaque]`
/// paths name; a type named by two fns keeps the first.
fn crate_definers<'n, 'a, S: NamesSource<'n>>(names: S, krate: usize, alloc: Alloc<'a>) -> Result<Vec<(LibKey, LibKey), Alloc<'a>>, DefinerMiss> {
    let mut rows: Vec<(LibKey, LibKey), Alloc<'a>> = Vec::new_in(alloc);
    let read = names.each_item(krate, &mut |key, module| {
        if names.kind_of_item(key) != Some(blaze_surface::ItemKind::Function) {
            return;
        }
        let Some(it) = names.text_of(key) else { return };
        for path in define_opaque_paths(it.text) {
            if let Some(ty) = named_type(names, module, path)
                && !rows.iter().any(|row| row.0 == ty)
            {
                rows.push((ty, key));
            }
        }
    });
    if !read {
        return Err(DefinerMiss::NotRead);
    }
    Ok(rows)
}

/// What `path`, written in `module`, names in the type namespace. A path with generic
/// arguments or a `$crate` names nothing here: `#[define_opaque]` takes plain paths to aliases
/// (rustc_attr_parsing, `define_opaque`). A path of more than [`MOST_SEGMENTS`] segments is
/// refused as naming nothing (rule 18: the segments are held on the stack).
fn named_type<'n, S: NamesSource<'n>>(names: S, module: LibKey, path: &str) -> Option<LibKey> {
    use blaze_scope::PathKind;
    let path = path.trim();
    let absolute = path.starts_with("::");
    let mut held: [&str; MOST_SEGMENTS] = [""; MOST_SEGMENTS];
    let mut len = 0usize;
    for s in path.split("::").map(str::trim).filter(|s| !s.is_empty()) {
        if len == MOST_SEGMENTS || s.contains(['<', '>', '$', ' ']) || (len > 0 && matches!(s, "crate" | "self")) {
            return None;
        }
        held[len] = s;
        len += 1;
    }
    let segments = &held[..len];
    let (kind, rest) = match *segments.first()? {
        _ if absolute => (PathKind::Abs, 0),
        "crate" => (PathKind::Crate, 1),
        "self" => (PathKind::SelfMod, 1),
        "super" => {
            let supers = segments.iter().take_while(|s| **s == "super").count();
            (PathKind::Super(u8::try_from(supers).ok()?), supers)
        }
        _ => (PathKind::Plain, 0),
    };
    let tail = &segments[rest..];
    if tail.is_empty() {
        return None;
    }
    let written = blaze_scope::names::Written { kind, segments: tail };
    Some(names.walk(module, blaze_surface::Namespace::Types, written).ok()?.whole(tail.len())?.key)
}

/// Each path an item's `#[define_opaque(..)]` attributes list, in order: the text between the
/// parentheses of each `#[define_opaque(` (spaces allowed between the tokens), split at its
/// top-level commas. Words of the attribute's name elsewhere (a comment, a string, a call of a
/// macro of that name) are not an attribute and give nothing.
fn define_opaque_paths(text: &str) -> impl Iterator<Item = &str> {
    const NAME: &str = "define_opaque";
    let mut from = 0usize;
    let mut args: Option<core::str::Split<'_, char>> = None;
    core::iter::from_fn(move || {
        loop {
            if let Some(split) = args.as_mut() {
                if let Some(a) = split.find(|a| !a.trim().is_empty()) {
                    return Some(a);
                }
                args = None;
            }
            let at = from + text.get(from..)?.find(NAME)?;
            from = at + NAME.len();
            let before = text[..at].trim_end();
            let after = text[from..].trim_start();
            if !before.ends_with('[') || !before[..before.len() - 1].trim_end().ends_with('#') || !after.starts_with('(') {
                continue;
            }
            let inner = &after[1..];
            let Some(close) = inner.find(')') else { return None };
            args = Some(inner[..close].split(','));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{ALIAS_OPAQUE, define_opaque_paths, of_alias};

    /// The paths of `#[define_opaque(..)]` as rustc reads them: one or several, any spacing; the
    /// name in a doc comment or a call (`define_opaque!(X)`) is no attribute. The site: std
    /// backtrace.rs:435 `#[define_opaque(LazyResolve)]`.
    #[test]
    fn a_define_opaque_attribute_lists_its_paths() {
        let paths = |t: &'static str| define_opaque_paths(t).map(str::trim).collect::<std::vec::Vec<_>>();
        assert_eq!(paths("#[define_opaque(LazyResolve)]\npub(super) fn lazy_resolve() {}"), ["LazyResolve"]);
        assert_eq!(paths("# [ define_opaque ( a::B , super::C, ) ]\nfn f() {}"), ["a::B", "super::C"]);
        assert_eq!(paths("#[inline]\n#[define_opaque(A)]\n#[define_opaque(crate::B)]\nfn f() {}"), ["A", "crate::B"]);
        // The twins: no attribute.
        assert!(paths("/// see define_opaque(A)\nfn f() { define_opaque!(A); }").is_empty());
        assert!(paths("fn f() {}").is_empty());
        println!("define_opaque paths: one, several, two attributes; none in a comment or a call");
    }

    /// An alias's opaque row says so and keeps its number; a fn's never does. A row that lost
    /// the bit would send the auto-trait question to a fn body that does not exist (the alias
    /// has none); one that lost the number would make two `impl`s of one alias one type.
    #[test]
    fn an_alias_opaque_row_is_told_from_a_fn_one() {
        for j in [0u32, 1, 7, 1000] {
            assert!(of_alias(ALIAS_OPAQUE | j), "alias opaque {j}");
            assert_eq!((ALIAS_OPAQUE | j) & !ALIAS_OPAQUE, j, "alias opaque {j} keeps its number");
            assert!(!of_alias(j), "fn opaque {j}");
        }
        println!("tait rows: alias opaques 0, 1, 7, 1000 told from fn opaques, numbers kept");
    }
}
