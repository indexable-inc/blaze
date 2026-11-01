//! `large_stack_frames`: a function, or a closure in it, whose stack frame is larger than its
//! limit, on a fiber whose stack is fixed.
//!
//! Follows clippy's `clippy_lints/src/large_stack_frames.rs` (MIT or Apache-2.0), read from
//! indexable-inc's fork at c2308e38 (nightly-2026-05-27), the nearest checkout to the pinned
//! nightly's (rust-lang/rust 12433c30). As in clippy: a function written in the file with a
//! body, its frame the sizes of its MIR's locals as the front end lays them out, linted at its
//! name when that passes the limit, its largest locals named. Unlike clippy:
//!
//! - the limit is derived from the stack a pool fiber runs on
//!   ([`FIBER_STACK`](super::large_stack_arrays::FIBER_STACK)): 1/32 of it, so a chain of 32
//!   such frames fits; `stack-size-threshold` in `clippy.toml` still sets it;
//! - the frame is not the sum of every local: locals that never hold storage at once share a
//!   slot, as the code generator shares them (`hir-ty-codegen/overlap.rs`, from
//!   [`FrameStorage::frame_storage_conflicts`], a named local keeping its slot through its
//!   scope as in a build with debug info), packed by the code generator's own
//!   [`share_greedily`];
//! - the closures of the body are frames of their own, each linted at the closure;
//! - a body one of whose locals has no layout the front end can compute (of a generic
//!   parameter, or a type that does not lower) is left alone, never taken as small.
//!
//! The body measured is the runtime one (drops elaborated, before MIR optimizations), what the
//! code generator starts from.

use hir::HirDisplay as _;
use hir_def::DefWithBodyId;
use hir_ty::{
    InferBodyId,
    db::HirDatabase as _,
    mir::{LocalId, MirBody},
};
use hir_ty_mir::mir::{FrameStorage as _, share_greedily};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasName as _},
};

use super::{
    Implemented,
    large_stack_arrays::{FIBER_STACK, FIBER_STACK_NAME, size_text},
};
use crate::check::Cx;

const LINT: Implemented = Implemented::LargeStackFrames;

/// The share of a fiber's stack one frame may take.
const SHARE: u64 = 32;

/// The limit when `clippy.toml` sets none.
pub(super) const DEFAULT_LIMIT: u64 = FIBER_STACK / SHARE;

/// How many of a frame's largest locals the note names.
const NAMED: usize = 3;

/// How many closures deep the walk of a function's closures goes.
const DEEPEST: u32 = 16;

/// A frame's size, and its largest locals with the size of each.
struct Frame {
    size: u64,
    largest: Vec<(LocalId, u64)>,
}

/// One frame over the limit, before it is reported.
struct Over {
    range: TextRange,
    /// The function, or the closure in it, as the message names it.
    what: String,
    size: u64,
    /// What the note says of each of the largest locals.
    largest: Vec<String>,
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(function) = ast::Fn::cast(node) else { return };
    if function.body().is_none() || !cx.is_written(node) {
        return;
    }
    let Some(name) = function.name() else { return };
    let configured = cx.conf().int("stack-size-threshold", -1);
    let limit = u64::try_from(configured).unwrap_or(DEFAULT_LIMIT);
    let mut over = find(cx, function, name, limit);
    over.sort_by_key(|it| (it.range.start(), it.range.end()));
    let why = match configured {
        0.. => format!("`stack-size-threshold` in `clippy.toml` ({})", size_text(limit)),
        _ => format!(
            "{}: 1/{SHARE} of a pool fiber's stack (`{FIBER_STACK_NAME}`, {})",
            size_text(limit),
            size_text(FIBER_STACK)
        ),
    };
    for it in over {
        let span = cx.span(it.range);
        let message = format!("{} has a stack frame of {}, over {why}", it.what, size_text(it.size));
        let diagnostic = cx.lint(LINT, node, span, message);
        diagnostic.note(format!("its largest locals: {}", it.largest.join(", ")));
        diagnostic.note(
            "measured on the body before MIR optimizations, locals that never hold storage at once \
             sharing a slot as the code generator shares them",
        );
        diagnostic.help(
            "box the large values (`Box::new`, `vec![..].into_boxed_slice()`), or split the work so they \
             are not all in one frame",
        );
    }
}

/// The frames of `function`, named `name`, and of the closures in it, over `limit`.
fn find<'t>(cx: &Cx<'_, 't>, function: ast::Fn<'t>, name: ast::Name<'t>, limit: u64) -> Vec<Over> {
    // alloc: the frames over the limit of one function, reported at the edge
    let mut over = Vec::new();
    let Some(f) = cx.fn_def(function) else { return over };
    let Some(analysis) = cx.analysis() else { return over };
    // The walk of a body meets the functions nested in it, each linted in its own body's query.
    if analysis.def() != hir::DefWithBody::from(f) {
        return over;
    }
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return over };
    let db = cx.db();
    let Ok(mir) = db.runtime_mir_body(InferBodyId::DefWithBodyId(def)) else { return over };
    let named = name.text();
    let mut bodies = vec![(None, mir, 0u32)]; // alloc: the function's body and its closures', a few
    while let Some((closure, mir, depth)) = bodies.pop() {
        if depth < DEEPEST {
            for &it in &mir.closures {
                if let Ok(body) = db.runtime_mir_body_for_closure(it) {
                    bodies.push((Some(it.loc(db).expr), body, depth + 1));
                }
            }
        }
        let Some(frame) = frame(cx, analysis, mir) else { continue };
        if frame.size <= limit {
            continue;
        }
        let (range, what) = match closure {
            None => (cx.range(name.syntax()), format!("`{named}`")),
            Some(expr) => (analysis.expr_range(expr), format!("this closure in `{named}`")),
        };
        let Some(range) = range else { continue };
        let largest = frame.largest.iter().map(|&(local, size)| say_local(cx, analysis, mir, local, size)).collect();
        over.push(Over { range, what, size: frame.size, largest });
    }
    over
}

/// The frame of `mir`, a body of `analysis`'s function: `None` when a local's layout is unknown.
fn frame<'db>(cx: &Cx<'db, '_>, analysis: &hir::BodyAnalysis<'db>, mir: &MirBody<'db>) -> Option<Frame> {
    let db = cx.db();
    // alloc: the sized locals of one body
    let mut sized: Vec<(LocalId, u64, u64)> = Vec::with_capacity(mir.locals.len());
    for (local, decl) in mir.locals.iter() {
        let layout = analysis.ty(decl.ty.as_ref()).layout(db).ok()?;
        if layout.size() > 0 {
            sized.push((local, layout.size(), layout.align().max(1)));
        }
    }
    sized.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.into_raw().into_u32().cmp(&b.0.into_raw().into_u32())));
    let locals: Vec<LocalId> = sized.iter().map(|it| it.0).collect(); // alloc: as `sized`
    let named = |local: LocalId| mir.binding_locals.iter().any(|(_, &it)| it == local);
    let storage = mir.frame_storage_conflicts(&locals, named);
    // alloc: as `sized`
    let members: Vec<(usize, u64)> = sized.iter().enumerate().map(|(at, &(_, size, _))| (at, size)).collect();
    let size = blaze_mem::frame::task(blaze_mem::Tag::Diagnostics, |scratch| {
        let conflicts = storage.matrix_in(sized.len(), scratch);
        let shared = share_greedily(&members, &conflicts, scratch);
        // Each slot: its size (its first local's, the largest) and its alignment, the largest
        // of its locals'.
        // alloc: the slots of one body
        let mut slots: Vec<(u64, u64)> =
            shared.first.iter().map(|&first| (sized[first as usize].1, 1)).collect();
        for (at, &group) in shared.group_of.iter().enumerate() {
            let slot = &mut slots[group as usize];
            slot.0 = slot.0.max(sized[at].1);
            slot.1 = slot.1.max(sized[at].2);
        }
        slots.iter().map(|&(size, align)| size.next_multiple_of(align)).sum::<u64>()
    });
    let largest = sized.iter().take(NAMED).map(|&(local, size, _)| (local, size)).collect();
    Some(Frame { size, largest })
}

/// A local as the note names it: the binding it holds, or the type of a temporary.
fn say_local<'db>(
    cx: &Cx<'db, '_>,
    analysis: &hir::BodyAnalysis<'db>,
    mir: &MirBody<'db>,
    local: LocalId,
    size: u64,
) -> String {
    let db = cx.db();
    match mir.binding_locals.iter().find(|&(_, &it)| it == local) {
        Some((binding, _)) => format!("`{}` ({})", analysis.body()[binding].name.as_str(), size_text(size)),
        None => {
            let ty = analysis.ty(mir.locals[local].ty.as_ref());
            let target = cx.krate().to_display_target(db);
            format!("a temporary of `{}` ({})", ty.display(db, target), size_text(size))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::more_rules_tests::lint_only;

    const LINT: &str = "large_stack_frames";
    const FOUND: &str = "has a stack frame of";

    #[test]
    fn a_frame_over_the_fibers_share_is_found_with_its_largest_locals() {
        let found = lint_only(
            LINT,
            r#"
pub struct Page { bytes: [u8; 1 << 20] }
pub fn fill(_: &mut Page) {}
pub fn keep<T>(_: &T) {}
pub fn big() {
    let mut page = Page { bytes: [0; 1 << 20] };
    fill(&mut page);
    keep(&page);
}
pub fn small() {
    let buf = [0u8; 4096];
    keep(&buf);
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 1, "{found}");
        assert!(found.contains("`big` has a stack frame of "), "{found}");
        assert!(
            found.contains(", over 512 KiB: 1/32 of a pool fiber's stack (`blaze_graph::sched::FIBER_STACK`, 16 MiB)"),
            "{found}"
        );
        assert!(found.contains("its largest locals: `page` (1 MiB)"), "{found}");
    }

    /// Two large locals whose storage never overlaps share one slot, as the code generator
    /// lays them out: the frame is one of them, not both.
    #[test]
    fn locals_that_never_hold_storage_at_once_share_a_slot() {
        let found = lint_only(
            LINT,
            r#"
pub fn take(_: &[u8; 400000]) {}
pub fn apart(flag: bool) {
    if flag {
        let a = [0u8; 400000];
        take(&a);
    } else {
        let b = [1u8; 400000];
        take(&b);
    }
}
pub fn together() {
    let a = [0u8; 400000];
    let b = [1u8; 400000];
    take(&a);
    take(&b);
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 1, "{found}");
        assert!(found.contains("`together` has a stack frame of"), "{found}");
    }

    #[test]
    fn a_generic_frame_whose_layout_is_unknown_is_left_alone_not_taken_as_small() {
        let found = lint_only(
            LINT,
            r#"
pub fn keep<T>(_: &T) {}
pub fn generic<T: Copy>(x: T) -> T {
    let many = [x; 1 << 20];
    keep(&many);
    x
}
pub fn sized() {
    let big = generic([0u8; 1 << 20]);
    keep(&big);
}
"#,
        );
        assert!(!found.contains("`generic` has a stack frame"), "a layout the front end can't compute is no finding:\n{found}");
        assert!(found.contains("`sized` has a stack frame of"), "{found}");
    }

    #[test]
    fn a_closures_frame_and_a_frame_written_in_a_macro_call_are_found() {
        let found = lint_only(
            LINT,
            r#"
mod pages { pub type Page = [u64; 1 << 17]; }
use pages::Page as Block;
macro_rules! item { ($($t:tt)*) => { $($t)* }; }
pub fn keep<T>(_: &T) {}
item! {
    pub fn handed() { let b: Block = [0; 1 << 17]; keep(&b); }
}
pub fn outer() {
    let f = || { let b: Block = [1; 1 << 17]; keep(&b); };
    f()
}
"#,
        );
        assert!(found.contains("`handed` has a stack frame of "), "{found}");
        assert!(found.contains("this closure in `outer` has a stack frame of "), "{found}");
        assert!(!found.contains("warning: `outer` has a stack frame"), "the closure's locals are its own frame:\n{found}");
    }

    #[test]
    fn stack_size_threshold_in_clippy_toml_sets_the_limit() {
        let conf = crate::conf::Conf { table: "stack-size-threshold = 1024".parse().unwrap(), ..Default::default() };
        let found = crate::tests::lint_with(
            &super::super::more_rules_tests::fixture("pub fn keep<T>(_: &T) {}\npub fn f() { let buf = [0u8; 4096]; keep(&buf); }\n"),
            "-A clippy::all -W clippy::large_stack_frames",
            conf,
        );
        assert!(found.contains("over `stack-size-threshold` in `clippy.toml` (1 KiB)"), "{found}");
    }
}
