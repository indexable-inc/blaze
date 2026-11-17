//! The paths a lowering left: each answered through the world's names (blaze-scope's, behind
//! [`Names`]), or [`Answer::Need`] when the world does not know yet. Locals were resolved by
//! the lowering; items declared in the body's own blocks are tried here first, through the
//! scope chain each path carries.
//!
//! A path's namespace is how it is used: a type's or a struct expression's or pattern's path
//! is in the type namespace, an expression's or a tuple-struct or unit pattern's in the value
//! namespace, a macro call's in the macro namespace. The walk is one pass over the columns, no
//! recursion. Its answer is a resolution per path and the read list: every question asked,
//! with its answer, so the result is valid while every answer still holds.

use blaze_own::{AVec, Alloc, Vec};
use blaze_typeck::ItemKey;

use crate::columns::{TextAt, View};
use crate::form::*;

/// A namespace of names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Ns {
    Type,
    Value,
    Macro,
}

/// What the world says of a question.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Found(ItemKey),
    /// Nothing by that name: an error the checker reports.
    Absent,
    /// Not known yet: the caller fetches it and asks again.
    Need,
}

/// The world's names, as blaze-scope answers them.
pub trait Names {
    /// The item `segments` names, in `ns` (for the last segment; the others are in the type
    /// namespace), read where `crate::mark_site::site_of` of `mark` (the first word's mark,
    /// [`View::mark_of_path`]) says: the body's module unless an expansion's mark says
    /// otherwise.
    fn lookup(&mut self, ns: Ns, absolute: bool, segments: &[&str], mark: Option<crate::mark_site::TextMark>) -> Answer;
    /// The item that is `lang`.
    fn lang(&mut self, lang: LangItem) -> Answer;
}

/// Where a path goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Item(ItemKey),
    /// An item declared in one of the body's blocks: its `LocalItem` row.
    LocalItem(RowIdx),
    Absent,
}

/// One path's resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub path: PathIdx,
    pub ns: Ns,
    pub target: Target,
}

/// One question asked of the world, and its answer: an entry of the read list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    /// The path at `path`, asked in `ns`.
    Path { path: PathIdx, ns: Ns, answer: Answer },
    Lang { lang: LangItem, answer: Answer },
}

/// Every path's resolution and the read list.
pub struct Resolution<'a> {
    pub paths: AVec<'a, Resolved>,
    pub reads: AVec<'a, Read>,
}

/// Why the paths are not all resolved.
#[derive(Debug)]
pub enum Unresolved<'a> {
    /// The world did not know these yet; the caller fetches them and resolves again.
    Need(AVec<'a, Read>),
    Bad(BadRow),
    /// A name's range is no text of the record.
    Text(TextAt),
}

impl From<BadRow> for Unresolved<'_> {
    fn from(b: BadRow) -> Self {
        Unresolved::Bad(b)
    }
}

impl From<TextAt> for Unresolved<'_> {
    fn from(t: TextAt) -> Self {
        match t {
            TextAt::Row(b) => Unresolved::Bad(b),
            t => Unresolved::Text(t),
        }
    }
}

/// The namespace each path is used in, by one pass over the columns that hold paths.
fn uses(v: &View<'_>, ns: &mut [Option<Ns>]) -> Result<(), BadRow> {
    let mut set = |p: PathIdx, n: Ns| {
        if let Some(slot) = ns.get_mut(p.0 as usize) {
            *slot = Some(n);
        }
    };
    for i in 0..v.exprs.len() {
        match v.expr(ExprId(i as u32))? {
            Expr::Path(p) => set(p, Ns::Value),
            Expr::Struct { path, .. } => set(path, Ns::Type),
            Expr::MacroCall { row } => set(v.macro_call(v.row(row)?)?.path, Ns::Macro),
            _ => {}
        }
    }
    for i in 0..v.pats.len() {
        match v.pat(PatId(i as u32))? {
            Pat::Path(p) | Pat::TupleStruct { path: p, .. } => set(p, Ns::Value),
            Pat::Struct { path, .. } => set(path, Ns::Type),
            Pat::MacroCall { row } => set(v.macro_call(v.row(row)?)?.path, Ns::Macro),
            _ => {}
        }
    }
    for i in 0..v.types.len() {
        match v.ty(TypeId(i as u32))? {
            TypeRef::Path(p) => set(p, Ns::Type),
            TypeRef::Macro(row) => set(v.macro_call(v.row(row)?)?.path, Ns::Macro),
            _ => {}
        }
    }
    Ok(())
}

/// The item a one-segment path names among the body's own, through its scope chain.
pub(crate) fn local_item(v: &View<'_>, scope: Option<ScopeId>, name: u64) -> Result<Option<RowIdx>, BadRow> {
    let mut at = scope;
    // A chain is at most as long as the scopes column: a cycle is a malformed record.
    for _ in 0..=v.scopes.len() {
        let Some(s) = at else { return Ok(None) };
        let scope = v.scope(s)?;
        for i in scope.items.start..scope.items.start + scope.items.len {
            let item = v.local_item(v.row(RowIdx(i))?)?;
            if let Some(n) = item.name
                && v.name(n)? == name
            {
                return Ok(Some(RowIdx(i)));
            }
        }
        at = scope.parent;
    }
    Err(BadRow { column: "scopes", tag: u16::MAX })
}

/// Resolves every path of `v` through `names`, in `alloc`.
pub fn resolve<'a>(v: &View<'_>, names: &mut dyn Names, alloc: Alloc<'a>) -> Result<Resolution<'a>, Unresolved<'a>> {
    let mut ns: AVec<'_, Option<Ns>> = Vec::from_elem_in(None, v.paths.len(), alloc);
    uses(v, &mut ns)?;
    let mut out = Resolution { paths: Vec::new_in(alloc), reads: Vec::new_in(alloc) };
    let mut need: AVec<'a, Read> = Vec::new_in(alloc);
    let mut segs: AVec<'_, &str> = Vec::new_in(alloc);
    for (i, use_) in ns.iter().enumerate() {
        let Some(n) = *use_ else { continue };
        let p = PathIdx(i as u32);
        let path = v.path(p)?;
        let (target, read) = if let Some(lang) = path.lang {
            let answer = names.lang(lang);
            (answer, Read::Lang { lang, answer })
        } else {
            segs.clear();
            let rows = v.segments(path)?;
            let mut name_hash = None;
            for &row in rows {
                let s = Segment::decode(row)?;
                let word = match (s.kind, s.name) {
                    (SegKind::Ident, Some(nm)) => {
                        name_hash = Some(v.name(nm)?);
                        v.name_text(nm)?
                    }
                    (SegKind::SelfMod, _) => "self",
                    (SegKind::Super, _) => "super",
                    (SegKind::Crate, _) => "crate",
                    (SegKind::SelfTy, _) => "Self",
                    (SegKind::DollarCrate, _) => "$crate",
                    // `<T as Tr>::x` is an associated item: the checker's, by the type.
                    (SegKind::Anchor, _) | (SegKind::Ident, None) => {
                        segs.clear();
                        break;
                    }
                };
                segs.push(word);
            }
            if segs.is_empty() {
                continue;
            }
            // A body's own items come first, by name; macros are never body items here.
            if rows.len() == 1
                && !path.absolute
                && n != Ns::Macro
                && let Some(h) = name_hash
                && let Some(row) = local_item(v, path.scope, h)?
            {
                out.paths.push(Resolved { path: p, ns: n, target: Target::LocalItem(row) });
                continue;
            }
            let answer = names.lookup(n, path.absolute, &segs, v.mark_of_path(p)?);
            (answer, Read::Path { path: p, ns: n, answer })
        };
        match target {
            Answer::Need => need.push(read),
            Answer::Found(k) => {
                out.reads.push(read);
                out.paths.push(Resolved { path: p, ns: n, target: Target::Item(k) });
            }
            Answer::Absent => {
                out.reads.push(read);
                out.paths.push(Resolved { path: p, ns: n, target: Target::Absent });
            }
        }
    }
    if !need.is_empty() {
        return Err(Unresolved::Need(need));
    }
    Ok(out)
}
