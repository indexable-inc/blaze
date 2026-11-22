//! Body builders that also write the ranges of written text that is not an expression: a
//! turbofish argument, a path segment, a qualified path's anchor type and a method call's name
//! (piece obligation-range; NEVER COMPILED when written, 2026-11-16). The plain builders of
//! [`Bb`] write none of these ranges ("no range known"), so a body they build places every
//! finding as before; these are for the tests of where an unmet obligation is filed
//! (`suite::obligation_range`).
//!
//! Where each range is, so a test can say it without the builder (an expression is at its id,
//! a pattern at 1000 + its id, as [`Bb`] puts them): a turbofish argument at [`TYPE_ARG_AT`] +
//! its index in `type_lists`, a segment at [`SEGMENT_AT`] + its index in `segments`, an anchor
//! type at [`ANCHOR_AT`] + its path's index, a method call's name at [`METHOD_NAME_AT`] + the
//! call's id. Each is one byte long, so no two pieces of text overlap.

use super::Bb;
use crate::body::{Anchor, BodyPath, Expr, Segment};
use crate::ids::{ExprId, List, Name, PathIdx, Range, ScopeId};
use crate::portable::PTy;

/// The first turbofish argument's range is `TYPE_ARG_AT + its index in type_lists`.
pub const TYPE_ARG_AT: u32 = 2000;
/// A segment's range is `SEGMENT_AT + its index in segments`.
pub const SEGMENT_AT: u32 = 3000;
/// A qualified path's anchor type is at `ANCHOR_AT + the path's index`.
pub const ANCHOR_AT: u32 = 4000;
/// A method call's name is at `METHOD_NAME_AT + the call's id`.
pub const METHOD_NAME_AT: u32 = 5000;

/// What a ranged builder made, and the ranges of its pieces of text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ranged {
    pub expr: ExprId,
    /// The segment, or the method name, that names the item.
    pub name: Range,
    /// The first written generic argument, if any was written.
    pub first_arg: Option<Range>,
    /// A qualified path's anchor type.
    pub anchor: Option<Range>,
}

/// The one-byte range at `at`.
const fn one(at: u32) -> Range {
    Range { start: at, end: at + 1 }
}

impl Bb {
    /// `args` as a turbofish list, each with its range.
    fn ranged_types(&mut self, args: &[PTy]) -> (List, Option<Range>) {
        let start = self.type_lists.len() as u32;
        self.type_list_ranges.resize(self.type_lists.len(), Range::default());
        for &t in args {
            let at = TYPE_ARG_AT + self.type_lists.len() as u32;
            self.type_lists.push(t);
            self.type_list_ranges.push(one(at));
        }
        let first = if args.is_empty() { None } else { Some(one(TYPE_ARG_AT + start)) };
        (List { start, len: args.len() as u32 }, first)
    }

    /// One segment, with its range.
    fn ranged_segment(&mut self, name: &str, generic_args: List, has_args: bool) -> Range {
        let ix = self.segments.len() as u32;
        self.segment_ranges.resize(self.segments.len(), Range::default());
        self.segments.push(Segment { name: Name::of(name), generic_args, has_args });
        let r = one(SEGMENT_AT + ix);
        self.segment_ranges.push(r);
        r
    }

    /// `name::<args..>`, one segment, its arguments and its name ranged.
    pub fn path_args_ranged(&mut self, name: &str, args: &[PTy]) -> Ranged {
        let (generic_args, first_arg) = self.ranged_types(args);
        let start = self.segments.len() as u32;
        let name = self.ranged_segment(name, generic_args, true);
        let at = self.paths.len() as u32;
        self.paths.push(BodyPath { scope: ScopeId(0), segments: List { start, len: 1 }, lang: None, anchor: None });
        let expr = self.e(Expr::Path(PathIdx(at)));
        Ranged { expr, name, first_arg, anchor: None }
    }

    /// `segments[0]::segments[1]..`, each segment ranged; `name` is the last one's.
    pub fn path_ranged(&mut self, segments: &[&str]) -> Ranged {
        let start = self.segments.len() as u32;
        let mut name = Range::default();
        for s in segments {
            name = self.ranged_segment(s, List::EMPTY, false);
        }
        let at = self.paths.len() as u32;
        self.paths.push(BodyPath { scope: ScopeId(0), segments: List { start, len: segments.len() as u32 }, lang: None, anchor: None });
        let expr = self.e(Expr::Path(PathIdx(at)));
        Ranged { expr, name, first_arg: None, anchor: None }
    }

    /// `<ty as tr>::segments[0]..`, `tr` a trait's name in scope; the anchor type and every
    /// segment ranged.
    pub fn qpath_as_ranged(&mut self, ty: PTy, tr: &str, segments: &[&str]) -> Ranged {
        let tstart = self.segments.len() as u32;
        self.ranged_segment(tr, List::EMPTY, false);
        let tpath = self.paths.len() as u32;
        self.paths.push(BodyPath { scope: ScopeId(0), segments: List { start: tstart, len: 1 }, lang: None, anchor: None });
        let start = self.segments.len() as u32;
        let mut name = Range::default();
        for s in segments {
            name = self.ranged_segment(s, List::EMPTY, false);
        }
        let at = self.paths.len() as u32;
        let ty_range = one(ANCHOR_AT + at);
        let anchor = Some(Anchor { ty, trait_: Some(PathIdx(tpath)), ty_range });
        self.paths.push(BodyPath { scope: ScopeId(0), segments: List { start, len: segments.len() as u32 }, lang: None, anchor });
        let expr = self.e(Expr::Path(PathIdx(at)));
        Ranged { expr, name, first_arg: None, anchor: Some(ty_range) }
    }

    /// `receiver.name::<generic_args..>(args..)`, its name and turbofish ranged.
    pub fn method_ranged(&mut self, receiver: ExprId, name: &str, generic_args: &[PTy], args: &[ExprId]) -> Ranged {
        let (generic_args, first_arg) = self.ranged_types(generic_args);
        let args = self.exprs_list(args);
        let expr = self.e(Expr::MethodCall { receiver, name: Name::of(name), generic_args, args, scope: ScopeId(0) });
        let r = one(METHOD_NAME_AT + expr.0);
        // Ascending: a call's id is larger than every earlier one's.
        self.method_name_ranges.push((expr, r));
        Ranged { expr, name: r, first_arg, anchor: None }
    }
}
