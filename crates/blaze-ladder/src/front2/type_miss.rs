//! A written type whose path names nothing: rustc's E0412 ("cannot find type `T` in module
//! `a`"), at the path's item-relative range. One rule for a body's types (front2 body.rs
//! `BodyTypes::ty`) and a head's (the `Signature` task): the miss is the program's error only
//! when the names library DECIDED it (`NotFound`, `Private`, `Ambiguous`, `NotAModule`,
//! `SuperPastRoot`); a scope not read whole (`NotRead`, `Unbounded`) or a refused macro call of
//! the module (`Refused`) is no E0412, and the caller keeps its refusal naming that cause (rule
//! 3: never an error for a name that may exist). Its words are made here, from the names
//! alone, so a body's check and a head's `Signature` task say one miss alike: rustc's for the
//! plain case ("cannot find type `T` in module `wide::a` (segment 2)"), the reason beside the
//! segment for every other, as an E0425's are said (front2 path_miss_shown.rs). Segment shown as `` type `T` ``.

use std::alloc::Global;
use std::boxed::Box;
use std::vec::Vec;

use blaze_id::ItemKey as LibKey;
use blaze_scope::names::{PathError, Why};
use blaze_surface::Range;
use blaze_typeck as tc;

use super::keys::checker_of;
use super::source::NamesSource;
use crate::resolve::{Kind, Ns, Paths};

/// A written type of an item that names nothing: where (`at`, relative to the text the item's
/// types were read from), the segment that failed as written (`name`), the miss in the
/// checker's terms, and the finding's words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeMiss {
    pub at: Range,
    pub name: Box<str, Global>,
    pub miss: tc::PathMiss,
    pub words: Box<str, Global>,
}

impl TypeMiss {
    /// The miss of the library's refusal `e` of the type path written at `at` in `text` (the
    /// item's text, which `at` is relative to), or `None` where it is no E0412: a reason the
    /// library did not decide, a path not written in the item's own text (an expansion's, past
    /// its end), a qualified path (`<T as Tr>::X`), or a prefix that does not resolve again here.
    pub fn of<'n, S: NamesSource<'n>>(names: S, text: &str, e: PathError, at: Range) -> Option<TypeMiss> {
        let (why, reason) = match e.why {
            Why::NotFound => (tc::MissWhy::NotFound, "not found"),
            Why::Private => (tc::MissWhy::Private, "private"),
            Why::Ambiguous => (tc::MissWhy::Ambiguous, "ambiguous (two glob imports give it)"),
            Why::NotAModule => (tc::MissWhy::NotAModule, "after an item with no members"),
            Why::SuperPastRoot => (tc::MissWhy::SuperPastRoot, "`super` past the crate's root"),
            Why::Refused { .. } | Why::NotRead { .. } | Why::Unbounded | Why::Empty => return None,
        };
        // The path as written, its generic arguments cut (`a::T<u8>` is `a`, `T`).
        let written = text.get(at.start as usize..at.end as usize)?;
        let path = written.split('<').next()?.trim();
        let abs = path.starts_with("::");
        let mut segs: Vec<&str, Global> = Vec::new_in(Global);
        segs.extend(path.trim_start_matches("::").split("::").map(str::trim));
        if segs.iter().any(|s| s.is_empty() || !s.chars().all(|c| c == '_' || c.is_alphanumeric())) {
            return None;
        }
        let name = *segs.get(e.at)?;
        let segment = u32::try_from(e.at).ok()?;
        let (after, shown) = match e.at {
            0 => (None, None),
            n => {
                let (p, shown) = prefix(names, e.module, abs, &segs[..n])?;
                (Some(p), Some(shown))
            }
        };
        let words = match (why, shown) {
            (tc::MissWhy::NotFound, None) => format!("cannot find type `{name}` in this scope (segment {segment})"),
            (tc::MissWhy::NotFound, Some(p)) => format!("cannot find type `{name}` in {p} (segment {segment})"),
            (_, None) => format!("type `{name}` (segment {segment}) {reason} in this scope"),
            (_, Some(p)) => format!("type `{name}` (segment {segment}) {reason} on {p}"),
        };
        Some(TypeMiss { at, name: Box::from(name), miss: tc::PathMiss::At { segment, after, why }, words: words.into_boxed_str() })
    }

    /// The finding's range, in the checker's terms.
    pub fn range(&self) -> tc::Range {
        tc::Range { start: self.at.start, end: self.at.end }
    }
}

/// What the segments before the failing one named, read again in the type namespace a prefix is
/// read in, and in words: a module by its path (`` module `wide::a` ``), anything else by the
/// path as written; `None` where it does not resolve whole here.
fn prefix<'n, S: NamesSource<'n>>(names: S, module: LibKey, abs: bool, segs: &[&str]) -> Option<(tc::PathPrefix, String)> {
    let t = (Paths { names }).path(module, abs, segs, Ns::Type).ok()?;
    if t.consumed as usize != segs.len() {
        return None;
    }
    let k = checker_of(t.key);
    let written = segs.join("::");
    Some(match t.kind {
        Kind::Module => {
            let shown = match names.module_path(t.key) {
                Some(path) => format!("module `{path}`"),
                None => format!("the module `{written}`"),
            };
            (tc::PathPrefix::Module(k), shown)
        }
        Kind::Struct | Kind::Enum | Kind::Union => (tc::PathPrefix::Def(tc::Def::Adt(k)), format!("the type `{written}`")),
        Kind::Trait => (tc::PathPrefix::Def(tc::Def::Trait(k)), format!("the trait `{written}`")),
        _ => (tc::PathPrefix::Item(k), format!("the item `{written}`")),
    })
}
