//! `panic_in_drop`, as an extension: blaze's own lint of that name
//! (crates/blaze-lints/src/blaze/panic_in_drop.rs) written against `blaze-ext`, over the
//! body's MIR instead of its HIR. The worked example of
//! docs/v2/plan/design/extensions.md, and the crate its tests compile and run.
//!
//! The lint: an `impl Drop` whose `drop` can panic. `drop` runs while a panic unwinds too; a
//! second panic then aborts the process. Found: each call the body's MIR ends a block with
//! that is `unwrap`, `expect`, `unwrap_err` or `expect_err` of `Option` or `Result`, or a
//! call into the toolchain's panic machinery (what `panic!` and `assert!` expand to). A body
//! that calls `std::thread::panicking()` is taken as handling the unwinding case. Narrower
//! than the in-tree lint by what the built MIR does not show as a terminator: an index and a
//! division have no check there until blaze's later passes add one.
//!
//! The fact: [`MayPanic`], whether an item can panic by what it writes or by what it calls in
//! its crate, to a fixpoint over recursion. [`DEEP`] reports a `drop` that calls such an
//! item; it is `allow` unless the workspace turns it on.

#![no_std]

use blaze_ext::{
    Analysis, Body, Bytes, Callee, Cx, Extension, Fact, FactDecl, Level, Lint,
    mir::{MirSpan, TerminatorKind},
};

pub fn extension() -> Extension {
    Extension { name: "panic-in-drop", api: blaze_ext::VERSION, lints: &LINTS, facts: &FACTS }
}

const LINTS: [Lint; 2] = [SHALLOW, DEEP];
const FACTS: [FactDecl; 1] = [FactDecl::of::<MayPanicOf>()];

const SHALLOW: Lint = Lint {
    name: "panic_in_drop",
    level: Level::Warn,
    summary: "an `impl Drop` whose `drop` can panic",
    requires: &[],
    run: shallow,
};

const DEEP: Lint = Lint {
    name: "panic_in_drop_deep",
    level: Level::Allow,
    summary: "an `impl Drop` whose `drop` calls a function of its crate that can panic",
    requires: &[MayPanic::NAME],
    run: deep,
};

const NOTE: &str = "`drop` also runs while a panic unwinds, and a second panic then aborts the process; \
                    handle the error (log it, or keep it for an explicit `close` method that returns it), or \
                    skip the work when `std::thread::panicking()` is true";

/// What a call of `callee` is, when calling it panics by what it is: as a message names it.
fn panics(callee: &Callee) -> Option<Panics<'_>> {
    let unwraps = matches!(callee.name(), "unwrap" | "expect" | "unwrap_err" | "expect_err")
        && (callee.is_std_method("option", "Option") || callee.is_std_method("result", "Result"));
    if unwraps {
        return Some(Panics::Named(callee.name()));
    }
    let machinery = callee.in_std("panicking")
        || (callee.in_std("rt") && callee.name().contains("panic"))
        || (callee.in_std("panic") && matches!(callee.name(), "panic_any" | "resume_unwind"));
    machinery.then_some(Panics::Machinery)
}

enum Panics<'c> {
    /// `unwrap`, `expect` and their `_err` forms.
    Named(&'c str),
    /// A call into `core::panicking` or std's `rt`.
    Machinery,
}

/// The calls of `body`, each with where its terminator is.
fn calls<'a>(body: &Body<'a, '_>) -> impl Iterator<Item = (MirSpan, &'a Callee)> {
    body.mir().basic_blocks.iter().filter_map(move |(block, data)| {
        let terminator = data.terminator.as_ref()?;
        let TerminatorKind::Call { .. } = terminator.kind else { return None };
        Some((terminator.span, body.callee(block)?))
    })
}

/// The `Self` type of the `Drop` impl whose `drop` this body is, if it is one that does not
/// handle unwinding itself.
fn dropped<'a>(body: &Body<'a, '_>) -> Option<&'a str> {
    let of = body.impl_of()?;
    if body.name() != "drop" || !of.is_std_trait("ops", "Drop") {
        return None;
    }
    let handles = calls(body).any(|(_, callee)| callee.is_std_free("thread", "panicking"));
    (!handles).then_some(of.self_ty())
}

fn shallow(cx: &mut Cx<'_>, body: &Body<'_, '_>) {
    let Some(of) = dropped(body) else { return };
    for (at, callee) in calls(body) {
        let report = match panics(callee) {
            Some(Panics::Named(name)) => {
                cx.report(at, format_args!("this `{name}` can panic in `Drop::drop` of `{of}`"))
            }
            Some(Panics::Machinery) => {
                cx.report(at, format_args!("this panic can panic in `Drop::drop` of `{of}`"))
            }
            None => continue,
        };
        report.note(format_args!("{NOTE}"));
    }
}

/// Whether an item can panic: by a call it writes ([`panics`]) or by calling an item of its
/// crate that can.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MayPanic(pub bool);

impl Fact for MayPanic {
    const NAME: &'static str = "may_panic";

    fn bottom() -> MayPanic {
        MayPanic(false)
    }

    fn encode(&self, out: &mut Bytes<'_>) {
        out.push(u8::from(self.0));
    }

    fn decode(bytes: &[u8]) -> Option<MayPanic> {
        match bytes {
            [0] => Some(MayPanic(false)),
            [1] => Some(MayPanic(true)),
            _ => None,
        }
    }
}

struct MayPanicOf;

impl Analysis for MayPanicOf {
    type Fact = MayPanic;

    const REQUIRES: &'static [&'static str] = &[];

    fn run(cx: &mut Cx<'_>, body: &Body<'_, '_>) -> MayPanic {
        let may = calls(body).any(|(_, callee)| {
            panics(callee).is_some()
                || callee.item().and_then(|it| cx.fact::<MayPanic>(it)).is_some_and(|it| it.0)
        });
        MayPanic(may)
    }
}

fn deep(cx: &mut Cx<'_>, body: &Body<'_, '_>) {
    let Some(of) = dropped(body) else { return };
    for (at, callee) in calls(body) {
        let may = callee.item().and_then(|it| cx.fact::<MayPanic>(it)).is_some_and(|it| it.0);
        if may && panics(callee).is_none() {
            let name = callee.name();
            cx.report(
                at,
                format_args!("`{name}` can panic, and is called in `Drop::drop` of `{of}`"),
            )
            .note(format_args!("{NOTE}"));
        }
    }
}
