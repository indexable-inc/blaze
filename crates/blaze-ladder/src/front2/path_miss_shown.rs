//! An E0425 finding's words ([`blaze_typeck::Because::Path`]): the segment that named nothing,
//! by its spelling and index, why, and what the segments before it named, so `Rc::new` reads
//! "cannot find `Rc` in this scope (segment 0)" or "cannot find `new` in the type alloc::rc::Rc
//! (segment 1)", and `crate::a::f` "cannot find `f` in module `wide::a` (segment 2)", never the
//! last segment's hash with "found None, expected None" (rule 19).

use blaze_typeck as tc;

use super::check::{Records, item_shown};
use super::source::NamesSource;
use super::spell::Spelling;

/// Why a path named nothing when a macro call of its module was refused: the name may be one
/// the call would have made, so the miss is a consequence of that refusal, not an error of its
/// own. The words after these name the call and why it was refused ([`refused_call`]): blaze-cli
/// body_types.rs reads a finding with them as "not checked" under that call, its cause.
pub const MACRO_REFUSED: &str = "unknown, because a macro call of its module was refused: ";

/// The refused call `call` and why, in words: the call's macro path and the matcher's reason
/// for a failed expansion (the names' failed-call note, [`NamesSource::failed_call`]), else the
/// refusal's own words. Never the key alone (rule 19).
fn refused_call<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, call: blaze_id::ItemKey, why: tc::Refusal) -> String {
    match why {
        tc::Refusal::UnknownEnv => "an `env!` of a variable this build could not answer".to_string(),
        tc::Refusal::BuildScriptOutputs => "`env!(\"OUT_DIR\")` of a package whose build script's outputs are missing (the build script has not run)".to_string(),
        tc::Refusal::Expansion => {
            let alloc = blaze_own::Alloc::global();
            match r.names.failed_call(call, alloc) {
                Some(row) if row.implicit => format!("the implicit `#[{}]` over `fn main` failed: {}", row.path, row.failed),
                Some(row) => format!("the call of `{}` failed: {}", row.path, row.failed),
                None => format!("macro call {call:x} failed, and the records hold no note of its reason"),
            }
        }
    }
}

/// The words of `miss`, whose failing segment the checker named `name`.
pub(super) fn path_miss_shown<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, spell: &Spelling<'_>, name: Option<tc::Name>, miss: tc::PathMiss) -> String {
    let seg = match name.map(|n| spell.get(n)) {
        Some(Ok(text)) => format!("`{text}`"),
        Some(Err(unspelled)) => format!("{unspelled:?}"),
        None => "a path with no segment".to_string(),
    };
    path_miss_words(r, &seg, miss)
}

/// The words of `miss`, its failing segment shown as `seg` (`` `f` ``, `` type `T` ``): the one
/// home of a miss's words, for a value path (E0425, [`path_miss_shown`]) and a written type
/// (E0412, front2 body.rs `TypeMiss`).
pub(super) fn path_miss_words<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, seg: &str, miss: tc::PathMiss) -> String {
    let (segment, after, why) = match miss {
        tc::PathMiss::Whole => return format!("{seg}: the path names nothing (this world does not say at which segment)"),
        tc::PathMiss::At { segment, after, why } => (segment, after, why),
    };
    let miss_why = why;
    let why = match why {
        tc::MissWhy::NotFound => "not found",
        tc::MissWhy::Private => "private",
        tc::MissWhy::Ambiguous => "ambiguous (two glob imports give it)",
        tc::MissWhy::NotAModule => "after an item with no members",
        tc::MissWhy::SuperPastRoot => "`super` past the crate's root",
        // The cause last, whole: what follows MACRO_REFUSED is the cause's words alone.
        tc::MissWhy::Refused { call, why } => return format!("{seg} (segment {segment}) {MACRO_REFUSED}{}", refused_call(r, call, why)),
        tc::MissWhy::ThroughUse => "a `use` or glob binding whose own path names no item",
        tc::MissWhy::NotAValue(k) => return format!("{seg} (segment {segment}) names {}, which is no value", item_shown(r, k)),
    };
    // rustc's words for the plain case ("cannot find function `f` in module `a`"), the segment's
    // index after them; every other reason says why beside the segment.
    match (miss_why, after) {
        (tc::MissWhy::NotFound, None) => format!("cannot find {seg} in this scope (segment {segment})"),
        (tc::MissWhy::NotFound, Some(p)) => format!("cannot find {seg} in {} (segment {segment})", prefix_shown(r, p)),
        (_, None) => format!("{seg} (segment {segment}) {why} in this scope"),
        (_, Some(p)) => format!("{seg} (segment {segment}) {why} on {}", prefix_shown(r, p)),
    }
}

/// A module by its path (`wide::a`), from the names source; a module with none (a block's) by
/// its [`item_shown`].
fn module_shown<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, k: tc::ItemKey) -> String {
    match r.keys.library_in(r.names, k).and_then(|lib| r.names.module_path(lib)) {
        Some(path) => format!("module `{path}`"),
        None => format!("the module {}", item_shown(r, k)),
    }
}

/// What a path's resolved prefix named, in words.
fn prefix_shown<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, p: tc::PathPrefix) -> String {
    match p {
        tc::PathPrefix::Module(k) => module_shown(r, k),
        tc::PathPrefix::Item(k) => format!("the item {}", item_shown(r, k)),
        // Rule 19: what `std` named, and that Rust's `std` has no name under the edition
        // (blaze-unit-graph implicit.rs: "There is no opt-in to Rust's `std` under the edition").
        tc::PathPrefix::Bstd { module, edition } => format!(
            "{} (note: `std` here names bstd, under edition {edition}; that edition gives Rust's `std` no name)",
            module_shown(r, module)
        ),
        tc::PathPrefix::Def(d) => match d {
            tc::Def::Adt(k) => format!("the type {}", item_shown(r, k)),
            tc::Def::Trait(k) => format!("the trait {}", item_shown(r, k)),
            tc::Def::Fn(k) | tc::Def::Const(k) | tc::Def::Static(k, _) => format!("the item {}", item_shown(r, k)),
            tc::Def::Ctor { adt, variant, .. } => format!("variant {variant} of {}", item_shown(r, adt)),
            tc::Def::TyParam(i) => format!("the generic parameter #{i}"),
            tc::Def::Prim(prim) => format!("the built-in type {prim:?}"),
            tc::Def::SelfTy => "`Self`".to_string(),
            tc::Def::Alias(k) => format!("the type alias {}", item_shown(r, k)),
        },
    }
}
