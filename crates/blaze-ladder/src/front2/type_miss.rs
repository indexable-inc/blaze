//! A written type whose path names nothing: rustc's E0412 ("cannot find type `T` in module
//! `a`"), at the path's item-relative range. One rule for a body's types (front2 body.rs
//! `BodyTypes::ty`) and a head's (the `Signature` task): the miss is the program's error only
//! when the names library DECIDED it (`NotFound`, `Private`, `Ambiguous`, `NotAModule`,
//! `SuperPastRoot`); a scope not read whole (`NotRead`, `Unbounded`) or a refused macro call of
//! the module (`Refused`) is no E0412, and the caller keeps its refusal naming that cause (rule
//! 3: never an error for a name that may exist). Its words are the E0425's
//! ([`super::path_miss_shown::path_miss_words`]), the type's segment shown as `` type `T` ``.

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

/// A written type of an item that names nothing: where (`at`, item-relative), the segment that
/// failed as written (`name`), and the miss in the checker's terms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeMiss {
    pub at: Range,
    pub name: Box<str, Global>,
    pub miss: tc::PathMiss,
}

impl TypeMiss {
    /// The miss of the library's refusal `e` of the type path written at `at` in `text` (the
    /// item's text, which `at` is relative to), or `None` where it is no E0412: a reason the
    /// library did not decide, a path not written in the item's own text (an expansion's, past
    /// its end), a qualified path (`<T as Tr>::X`), or a prefix that does not resolve again here.
    pub fn of<'n, S: NamesSource<'n>>(names: S, text: &str, e: PathError, at: Range) -> Option<TypeMiss> {
        let why = match e.why {
            Why::NotFound => tc::MissWhy::NotFound,
            Why::Private => tc::MissWhy::Private,
            Why::Ambiguous => tc::MissWhy::Ambiguous,
            Why::NotAModule => tc::MissWhy::NotAModule,
            Why::SuperPastRoot => tc::MissWhy::SuperPastRoot,
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
        let after = match e.at {
            0 => None,
            n => Some(prefix(names, e.module, abs, &segs[..n])?),
        };
        Some(TypeMiss { at, name: Box::from(name), miss: tc::PathMiss::At { segment, after, why } })
    }

    /// The finding's range, in the checker's terms.
    pub fn range(&self) -> tc::Range {
        tc::Range { start: self.at.start, end: self.at.end }
    }
}

/// What the segments before the failing one named, read again in the type namespace a prefix is
/// read in; `None` where it does not resolve whole here.
fn prefix<'n, S: NamesSource<'n>>(names: S, module: LibKey, abs: bool, segs: &[&str]) -> Option<tc::PathPrefix> {
    let t = (Paths { names }).path(module, abs, segs, Ns::Type).ok()?;
    if t.consumed as usize != segs.len() {
        return None;
    }
    let k = checker_of(t.key);
    Some(match t.kind {
        Kind::Module => tc::PathPrefix::Module(k),
        Kind::Struct | Kind::Enum | Kind::Union => tc::PathPrefix::Def(tc::Def::Adt(k)),
        Kind::Trait => tc::PathPrefix::Def(tc::Def::Trait(k)),
        _ => tc::PathPrefix::Item(k),
    })
}
