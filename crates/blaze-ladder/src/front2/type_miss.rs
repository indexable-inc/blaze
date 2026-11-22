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

/// Whether the names library DECIDED that the path names nothing (the module docs: only then
/// is a miss the program's error). [`TypeMiss::of`] maps exactly these reasons to words.
pub fn decided(e: PathError) -> bool {
    matches!(e.why, Why::NotFound | Why::Private | Why::Ambiguous | Why::NotAModule | Why::SuperPastRoot)
}

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
        TypeMiss::named(names, text, e, at, "type")
    }

    /// [`TypeMiss::of`] for a bound's trait path (rustc's E0405, "cannot find trait `Tr` in
    /// this scope"): the same rule, the same decided reasons, the words naming a trait.
    pub fn of_trait<'n, S: NamesSource<'n>>(names: S, text: &str, e: PathError, at: Range) -> Option<TypeMiss> {
        TypeMiss::named(names, text, e, at, "trait")
    }

    fn named<'n, S: NamesSource<'n>>(names: S, text: &str, e: PathError, at: Range, what: &str) -> Option<TypeMiss> {
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
            (tc::MissWhy::NotFound, None) => format!("cannot find {what} `{name}` in this scope (segment {segment})"),
            (tc::MissWhy::NotFound, Some(p)) => format!("cannot find {what} `{name}` in {p} (segment {segment})"),
            (_, None) => format!("{what} `{name}` (segment {segment}) {reason} in this scope"),
            (_, Some(p)) => format!("{what} `{name}` (segment {segment}) {reason} on {p}"),
        };
        Some(TypeMiss { at, name: Box::from(name), miss: tc::PathMiss::At { segment, after, why }, words: words.into_boxed_str() })
    }

    /// The finding's range, in the checker's terms.
    pub fn range(&self) -> tc::Range {
        tc::Range { start: self.at.start, end: self.at.end }
    }

    /// The miss of a written type as a fn's head keeps it ([`HeadMiss`]).
    pub fn kept(&self) -> HeadMiss {
        self.kept_as(MissOf::Type)
    }

    /// The miss as a fn's head keeps it, a type's (E0412) or a bound's trait's (E0405).
    pub fn kept_as(&self, of: MissOf) -> HeadMiss {
        HeadMiss { at: self.at, words: self.words.clone(), of }
    }
}

/// What a head's miss failed to name: a written type (E0412) or a bound's trait (E0405).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissOf {
    Type,
    Trait,
}

impl MissOf {
    /// Its finding's code.
    pub fn code(self) -> tc::DiagCode {
        match self {
            MissOf::Type => tc::DiagCode::UnresolvedType,
            MissOf::Trait => tc::DiagCode::UnresolvedTrait,
        }
    }
}

/// A signature's written type that names nothing, as its fn's head keeps it (front2 heads.rs
/// `Store::type_misses`, and the head's record, which a body task's store is filed from): where
/// (item-relative, in the fn's text) and the finding's words. The fn's OWN body check says it
/// as the head's E0412 (front2 check.rs), once; a caller is checked against the rest of the
/// head (the error type in the miss's place) and never says it. A bound whose trait names
/// nothing is the head's E0405 the same way, the head made without that bound. A record
/// carries no [`tc::PathMiss`]: nothing reads a head's miss but its finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadMiss {
    pub at: Range,
    pub words: Box<str>,
    pub of: MissOf,
}

impl HeadMiss {
    /// The finding's range, in the checker's terms.
    pub fn range(&self) -> tc::Range {
        tc::Range { start: self.at.start, end: self.at.end }
    }

    /// The finding: its code, range and words.
    pub fn finding(&self) -> (tc::DiagCode, tc::Range, Box<str>) {
        (self.of.code(), self.range(), self.words.clone())
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
