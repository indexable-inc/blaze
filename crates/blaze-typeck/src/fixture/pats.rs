//! Pattern builders [`Bb`] lacked: a tuple pattern, a tuple-struct or tuple-variant pattern by
//! path, a `mut` binding, and a match whose arms may have guards (piece RB7, the flow lowering's
//! peel under an ADT field; NEVER COMPILED when written, 2026-11-17). Each pattern is placed at
//! 1000 + its id, as [`Bb::bind`] places one.

use super::Bb;
use crate::body::{Arm, BindAnn, BodyPath, Expr, Pat, Segment};
use crate::ids::{ExprId, List, Mutbl, Name, PatId, PathIdx, Range, ScopeId};

impl Bb {
    fn push_pat(&mut self, p: Pat) -> PatId {
        let at = self.pats.len() as u32;
        self.pats.push(p);
        self.pat_ranges.push(Range { start: 1000 + at, end: 1001 + at });
        PatId(at)
    }

    fn pat_list(&mut self, elems: &[PatId]) -> List {
        let start = self.pat_lists.len() as u32;
        self.pat_lists.extend_from_slice(elems);
        List { start, len: elems.len() as u32 }
    }

    /// `(elems..)`, no `..`.
    pub fn tuple_pat(&mut self, elems: &[PatId]) -> PatId {
        let elems = self.pat_list(elems);
        self.push_pat(Pat::Tuple { elems, rest: None })
    }

    /// `alts[0] | alts[1] | ..`.
    pub fn or_pat(&mut self, alts: &[PatId]) -> PatId {
        let alts = self.pat_list(alts);
        self.push_pat(Pat::Or { alts })
    }

    /// `[elems..]`, the `..` (a `wild`, or `bind_sub(name, wild)`) at index `rest`.
    pub fn slice_pat(&mut self, elems: &[PatId], rest: Option<u32>) -> PatId {
        let elems = self.pat_list(elems);
        self.push_pat(Pat::Slice { elems, rest })
    }

    /// `segments[0]::segments[1]..(elems..)`: a tuple struct or tuple variant by its path, no
    /// `..`.
    pub fn variant_pat(&mut self, segments: &[&str], elems: &[PatId]) -> PatId {
        let start = self.segments.len() as u32;
        for s in segments {
            self.segments.push(Segment { name: Name::of(s), generic_args: List::EMPTY, has_args: false });
        }
        let path = PathIdx(self.paths.len() as u32);
        self.paths.push(BodyPath { scope: ScopeId(0), segments: List { start, len: segments.len() as u32 }, lang: None, anchor: None });
        let elems = self.pat_list(elems);
        self.push_pat(Pat::TupleStruct { path, elems, rest: None })
    }

    /// A `mut name` binding.
    pub fn bind_mut(&mut self, name: &str) -> PatId {
        self.push_pat(Pat::Bind { ann: BindAnn { by_ref: None, mutable: true }, name: Name::of(name), sub: None, ty: None })
    }

    /// `&inner` (`m` is `Not`) or `&mut inner` (`Mut`).
    pub fn ref_pat(&mut self, m: Mutbl, inner: PatId) -> PatId {
        self.push_pat(Pat::Ref { mutbl: m, inner })
    }

    /// A `ref name` (`m` is `Not`) or `ref mut name` (`Mut`) binding.
    pub fn bind_ref(&mut self, name: &str, m: Mutbl) -> PatId {
        self.push_pat(Pat::Bind { ann: BindAnn { by_ref: Some(m), mutable: false }, name: Name::of(name), sub: None, ty: None })
    }

    /// `match scrutinee { pat if guard => body, .. }`: each arm's guard, if any.
    pub fn match_guarded(&mut self, scrutinee: ExprId, arms: &[(PatId, Option<ExprId>, ExprId)]) -> ExprId {
        let start = self.arms.len() as u32;
        for &(pat, guard, body) in arms {
            self.arms.push(Arm { pat, guard, body });
        }
        self.e(Expr::Match { scrutinee, arms: List { start, len: arms.len() as u32 } })
    }
}
