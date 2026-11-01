//! `held_across_wait`: a value tied to its thread live across a call that may park the fiber
//! (AGENTS.md rule 22: a fiber may resume on another thread after any wait). The calls and what
//! is live across each are the may-suspend check's ([`hir_ty_suspend::suspend_points`]); a value
//! it already rejects (not `Send`, or `#[must_not_suspend]`: `why` is set) is its error and not
//! repeated here. What `Send` does not catch, and this lint does:
//!
//! - a reference lent into this thread's storage: the parameter of a closure handed to
//!   `LocalKey::{with, try_with, with_borrow, with_borrow_mut}` or `blaze_mem::frame::current`
//!   (a `&T` with `T: Sync` is `Send`, and still points at the first thread's slot);
//! - a value that is or holds `blaze_graph`'s `NoWait`, the marker of a region that must not
//!   wait;
//! - a value with a drop that holds a `LocalKey` (stdx's `lend` guard): it gives back to the slot
//!   of whichever thread it resumes on.
//!
//! A generic body is checked as its own crate instantiates it: an instance only another crate
//! makes is the may-suspend check's to report.

use base_db::Db;
use hir::HirDisplay as _;
use hir_def::{
    DefWithBodyId, FunctionId, HasModule as _,
    expr_store::Body,
    hir::{BindingId, Expr},
    signatures::FunctionSignature,
};
use hir_ty_suspend::HeldLocal;

use super::{
    Finding, Lint,
    alloc::written_callee,
    facts::{self, HOLDS_DEPTH, Parents},
};
use crate::check::Cx;

/// The methods of `LocalKey` that lend their closure a reference into the thread's slot, with
/// how a finding names each.
const LENDS: &[(&str, &str)] = &[
    ("with", "LocalKey::with"),
    ("try_with", "LocalKey::try_with"),
    ("with_borrow", "LocalKey::with_borrow"),
    ("with_borrow_mut", "LocalKey::with_borrow_mut"),
];

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(Lint::HeldAcrossWait) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

/// How a value held across a wait is tied to its thread.
enum Tie {
    NoWait,
    /// Lent by the function named.
    Lent(&'static str),
    /// A drop that gives back to a thread-local, of the type named.
    GivesBack(String),
}

/// The body linted and what its walk needs.
struct Walk<'a, 'db> {
    db: &'db Db,
    body: &'db Body,
    analysis: &'a hir::BodyAnalysis<'db>,
    parents: Parents,
    local_key: Option<hir::Adt>,
    drop: Option<hir::Trait>,
    target: hir::DisplayTarget,
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    let points = hir_ty_suspend::suspend_points(db, def.krate(db), def);
    if points.iter().all(|point| point.held.iter().all(|it| it.why.is_some())) {
        return found;
    }
    let body = analysis.body();
    let walk = Walk {
        db,
        body,
        analysis,
        parents: Parents::of(body),
        local_key: match cx.std_def(&["std", "thread", "LocalKey"]) {
            Some(hir::ModuleDef::Adt(it)) => Some(it),
            _ => None,
        },
        drop: cx.lang_trait(hir::LangItem::Drop),
        target: cx.krate().to_display_target(db),
    };
    for point in points.iter() {
        // alloc: what one finding names, a value or two
        let tied: Vec<String> = point
            .held
            .iter()
            .filter(|it| it.why.is_none())
            .filter_map(|held| Some(walk.say(held, &walk.tie(held)?)))
            .collect();
        let Some(first) = tied.first() else { continue };
        let Some(range) = facts::mir_range(analysis, point.call.span) else { continue };
        let message = match &*tied {
            [_] => format!("{first}, is tied to this thread and held across a call that may park the fiber"),
            _ => format!(
                "{} values tied to this thread are held across a call that may park the fiber: {}",
                tied.len(),
                tied.join("; ")
            ),
        };
        let note = format!(
            "the call parks the fiber through {}, and a parked fiber may resume on another thread (AGENTS.md rule 22), \
             where this still points into, or gives back to, the thread it was made on; its type is `Send`, so the \
             may-suspend check lets it through: end it before the call, or read the thread-local again after it",
            point.path_said(db)
        );
        facts::push(&mut found, Lint::HeldAcrossWait, range, message, Some(note));
    }
    found
}

impl<'db> Walk<'_, 'db> {
    fn tie(&self, held: &HeldLocal) -> Option<Tie> {
        let ty = self.analysis.ty(held.ty.as_ref());
        if facts::holds(self.db, &ty, HOLDS_DEPTH, &mut |adt| facts::adt_is(self.db, adt, "blaze_graph", "NoWait"))
            .is_some()
        {
            return Some(Tie::NoWait);
        }
        if let Some(binding) = held.binding
            && let Some(by) = self.lender_of(binding)
        {
            return Some(Tie::Lent(by));
        }
        let owned = ty.strip_references();
        if let (Some(key), Some(drop)) = (self.local_key, self.drop)
            && let Some(adt) = owned.as_adt()
            && owned.impls_trait(self.db, drop, &[])
            && facts::holds(self.db, &owned, HOLDS_DEPTH, &mut |it| it == key).is_some()
        {
            return Some(Tie::GivesBack(adt.name(self.db).as_str().to_owned()));
        }
        None
    }

    /// The function that lends `binding` into this thread's storage, when it is a parameter of
    /// a closure handed to one.
    fn lender_of(&self, binding: BindingId) -> Option<&'static str> {
        for (id, expr) in self.body.exprs() {
            let Expr::Closure { args, .. } = expr else { continue };
            let mut binds = false;
            for &arg in args {
                self.body.walk_bindings_in_pat(arg, |it| binds |= it == binding);
            }
            if !binds {
                continue;
            }
            let call = self.parents.of_expr(id)?;
            match self.body.expr(call) {
                Expr::Call { args, .. } | Expr::MethodCall { args, .. } if args.contains(&id) => {}
                _ => return None,
            }
            let (f, _) = written_callee(self.analysis.infer(), self.body, call)?;
            return self.lends(f);
        }
        None
    }

    fn lends(&self, f: FunctionId) -> Option<&'static str> {
        let name = FunctionSignature::of(self.db, f).name.as_str();
        if self.local_key.is_some() && facts::impl_adt(self.db, f) == self.local_key {
            return LENDS.iter().find(|(it, _)| *it == name).map(|(_, said)| *said);
        }
        let module = hir::Function::from(f).module(self.db);
        let in_frame = module.name(self.db).is_some_and(|it| it.as_str() == "frame")
            && facts::crate_is(self.db, module.krate(self.db), "blaze_mem");
        (in_frame && name == "current").then_some("blaze_mem::frame::current")
    }

    /// `held` as a finding names it, and why it is tied to the thread.
    fn say(&self, held: &HeldLocal, tie: &Tie) -> String {
        let ty = self.analysis.ty(held.ty.as_ref());
        let ty = ty.display(self.db, self.target);
        let what = match held.binding {
            Some(binding) => format!("`{}` (`{ty}`)", self.body[binding].name.as_str()),
            None => format!("a temporary `{ty}`"),
        };
        match tie {
            Tie::NoWait => format!("{what}, a `NoWait` region that must not wait"),
            Tie::Lent(by) => format!("{what}, lent by `{by}` from this thread's storage"),
            Tie::GivesBack(adt) => {
                format!("{what}, a `{adt}` whose drop gives back to a thread-local through the `LocalKey` it holds")
            }
        }
    }
}
