//! Where the code of an inlined body was written. A span of a body is of its owner's
//! expression store, unless it is a [`MirSpan::Inlined`]: that names one of the body's
//! [`Inlined::spans`], a span of the body of the callee its scope was inlined from, and the
//! scope names the call it was inlined at, a span of the body it was inlined into. That is
//! what rustc's inlined `SourceScope`s say, with `inlined` and `inlined_parent_scope`.

use base_db::Db;
use crate::{
    InferBodyId,
    db::HirDatabase,
    mir::{Inlined, InlinedBody, MirBody, MirSpan, location::track_caller, overflow},
};

/// The body whose expression store the spans of an inlined callee are of.
pub fn owner_of<'db>(db: &'db Db, callee: InlinedBody<'db>) -> InferBodyId<'db> {
    match callee {
        InlinedBody::Fn(func) => func.into(),
        InlinedBody::Closure(closure) => closure.loc(db).owner,
    }
}

/// The body `span` of `body` is written in, and the span there, which is never
/// [`MirSpan::Inlined`].
pub fn source_span<'db>(
    db: &'db Db,
    body: &MirBody<'db>,
    span: MirSpan,
) -> (InferBodyId<'db>, MirSpan) {
    match span {
        MirSpan::Inlined(at) => {
            let at = body.inlined.spans[at as usize];
            let scope = body.inlined.scopes[at.scope as usize];
            (owner_of(db, body.inlined.callees[scope.callee as usize].body), at.span)
        }
        span => (body.owner, span),
    }
}

/// Whether optional overflow checks (`Assert`s rustc's `is_optional_overflow_check` says are,
/// and the `overflow_checks` intrinsic) at `span` are written checked, as
/// [`overflow::body_written_checked`] says of a body: of an inlined one, the body it was
/// written in decides, as before it was inlined.
pub fn written_checked_at(db: &Db, body: &MirBody<'_>, span: MirSpan) -> bool {
    match span {
        MirSpan::Inlined(at) => {
            let scope = body.inlined.scopes[body.inlined.spans[at as usize].scope as usize];
            body.inlined.callees[scope.callee as usize].written_checked
        }
        _ => overflow::body_written_checked(db, body),
    }
}

/// What `Location::caller()` and a panic at a span of a body report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallerLocation<'db> {
    /// A span of the body of `owner`.
    At(InferBodyId<'db>, MirSpan),
    /// The location the caller of the body passes it: the body is `#[track_caller]`, and so
    /// is every body inlined around the span.
    Argument,
}

/// rustc's `Body::caller_location_span`: from `span` out through each `#[track_caller]`
/// callee it was inlined from, to the call, and out of the innermost callee that is not.
/// `tracked` says the body's own function is `#[track_caller]`.
pub fn caller_location<'db>(
    db: &'db Db,
    body: &MirBody<'db>,
    span: MirSpan,
    tracked: bool,
) -> CallerLocation<'db> {
    let tracks = |callee: u32| match body.inlined.callees[callee as usize].body {
        InlinedBody::Fn(func) => track_caller(db, func),
        InlinedBody::Closure(_) => false,
    };
    match caller_site(&body.inlined, span, tracks) {
        Site::Callee(callee, span) => {
            CallerLocation::At(owner_of(db, body.inlined.callees[callee as usize].body), span)
        }
        Site::Own(_) if tracked => CallerLocation::Argument,
        Site::Own(span) => CallerLocation::At(body.owner, span),
    }
}

/// Where [`caller_site`] stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Site {
    /// A span of the body of the callee `.0` of [`Inlined::callees`].
    Callee(u32, MirSpan),
    /// A span of the body itself.
    Own(MirSpan),
}

/// [`caller_location`] over the tables, `tracks` saying which callees are
/// `#[track_caller]`.
pub(super) fn caller_site(
    inlined: &Inlined<'_>,
    mut span: MirSpan,
    mut tracks: impl FnMut(u32) -> bool,
) -> Site {
    while let MirSpan::Inlined(at) = span {
        let at = inlined.spans[at as usize];
        let scope = inlined.scopes[at.scope as usize];
        if !tracks(scope.callee) {
            return Site::Callee(scope.callee, at.span);
        }
        span = scope.call_site;
    }
    Site::Own(span)
}
