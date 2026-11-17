//! An E0425 finding's words ([`blaze_typeck::Because::Path`]): the segment that named nothing,
//! by its spelling and index, why, and what the segments before it named, so `Rc::new` reads
//! "`Rc` (segment 0) not found in this scope" or "`new` (segment 1) not found on the type
//! alloc::rc::Rc", never the last segment's hash with "found None, expected None" (rule 19).

use blaze_typeck as tc;

use super::check::{Records, item_shown};
use super::source::NamesSource;
use super::spell::Spelling;

/// The words of `miss`, whose failing segment the checker named `name`.
pub(super) fn path_miss_shown<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, spell: &Spelling<'_>, name: Option<tc::Name>, miss: tc::PathMiss) -> String {
    let seg = match name.map(|n| spell.get(n)) {
        Some(Ok(text)) => format!("`{text}`"),
        Some(Err(unspelled)) => format!("{unspelled:?}"),
        None => "a path with no segment".to_string(),
    };
    let (segment, after, why) = match miss {
        tc::PathMiss::Whole => return format!("{seg}: the path names nothing (this world does not say at which segment)"),
        tc::PathMiss::At { segment, after, why } => (segment, after, why),
    };
    let why = match why {
        tc::MissWhy::NotFound => "not found",
        tc::MissWhy::Private => "private",
        tc::MissWhy::Ambiguous => "ambiguous (two glob imports give it)",
        tc::MissWhy::NotAModule => "after an item with no members",
        tc::MissWhy::SuperPastRoot => "`super` past the crate's root",
        tc::MissWhy::NotRead => "not read (the dependency's source is not read)",
        tc::MissWhy::Unbounded => "not found within the bound of a lookup through glob imports",
        tc::MissWhy::Refused => "unknown (a macro call of the module was refused)",
        tc::MissWhy::ThroughUse => "a `use` or glob binding whose own path names no item",
        tc::MissWhy::NotAValue(k) => return format!("{seg} (segment {segment}) names {}, which is no value", item_shown(r, k)),
    };
    match after {
        None => format!("{seg} (segment {segment}) {why} in this scope"),
        Some(p) => format!("{seg} (segment {segment}) {why} on {}", prefix_shown(r, p)),
    }
}

/// What a path's resolved prefix named, in words.
fn prefix_shown<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, p: tc::PathPrefix) -> String {
    match p {
        tc::PathPrefix::Module(k) => format!("the module {}", item_shown(r, k)),
        tc::PathPrefix::Item(k) => format!("the item {}", item_shown(r, k)),
        // Rule 19: what `std` named, and that Rust's `std` has no name under the edition
        // (blaze-unit-graph implicit.rs: "There is no opt-in to Rust's `std` under the edition").
        tc::PathPrefix::Bstd { module, edition } => format!(
            "the module {} (note: `std` here names bstd, under edition {edition}; that edition gives Rust's `std` no name)",
            item_shown(r, module)
        ),
        tc::PathPrefix::Def(d) => match d {
            tc::Def::Adt(k) => format!("the type {}", item_shown(r, k)),
            tc::Def::Trait(k) => format!("the trait {}", item_shown(r, k)),
            tc::Def::Fn(k) | tc::Def::Const(k) | tc::Def::Static(k, _) => format!("the item {}", item_shown(r, k)),
            tc::Def::Ctor { adt, variant, .. } => format!("variant {variant} of {}", item_shown(r, adt)),
            tc::Def::TyParam(i) => format!("the generic parameter #{i}"),
            tc::Def::Prim(prim) => format!("the built-in type {prim:?}"),
            tc::Def::SelfTy => "`Self`".to_string(),
        },
    }
}
