//! The lints that check what code says of its own speed against a profile of it running
//! ([`crate::profile`]): `blaze::stale_hot_marker`, a function marked hot that the profile
//! counts as cold, and `blaze::hot_marked_cold`, a `#[cold]` function the profile shows hot.
//! `blaze::profile_hot` is the driver's: a hot function's perf lints at its level
//! ([`crate::check`]). With no profile configured, none of them says anything.
//!
//! What the profile counts is a function's own code, every instance, closure and shim of it
//! ([`crate::profile::names`]). Two things follow, which narrow the lints:
//! - A call that LLVM inlined before the profile was taken (its pre-inliner under IR
//!   instrumentation) counts in the caller. So `#[inline(always)]` and `#[inline]` are not
//!   taken as marking a function hot (their calls are the ones inlined, and their own copy
//!   reads cold by construction), and `stale_hot_marker`'s message says a count of none may
//!   also mean every call was inlined.
//! - `#[inline(never)]` alone is not taken as marking a function cold: it also keeps a frame
//!   small or a symbol in the profile, which a hot function may want. `#[cold]` is the marker.

use hir_def::{AttrDefId, attrs::AttrFlags};
use syntax::view::{self as ast, AstNode as _, HasName as _};

use super::Lint;
use crate::{check::Cx, profile, query};

/// `blaze::stale_hot_marker` and `blaze::hot_marked_cold` on `f`.
pub(super) fn check_fn<'t>(cx: &mut Cx<'_, 't>, f: ast::Fn<'t>) {
    if !(cx.on_blaze(Lint::StaleHotMarker) || cx.on_blaze(Lint::HotMarkedCold))
        || cx.expansion().is_some()
        || !cx.is_written(f.syntax())
    {
        return;
    }
    let Some(loaded) = cx.conf().profile.as_ref() else { return };
    let db = cx.db();
    let Some(func) = cx.fn_def(f) else { return };
    let Some(id) = profile::function_id(func) else { return };
    // clone: the profile's settings and a refcount, once per function linted with a profile
    let key = query::ProfileKey::new(db, loaded.clone());
    let Some(hit) = query::profile_hits(db, cx.krate().base(), key).of(id) else { return };
    let file = loaded.file_name();
    let name = func.name(db);
    let name = name.as_str();
    let at = f.name().map_or(f.syntax(), |it| it.syntax());
    let Some(range) = cx.range(at) else { return };
    if cx.on_blaze(Lint::StaleHotMarker)
        && hit.cold
        && let Some(owner) = ast::AnyHasAttrs::cast(f.syntax())
        && let Some(marker) = cx.marks_hot(owner)
    {
        let counted = match hit.count {
            0 => "counts none of its code: it never ran, or every call to it was inlined before the profile was taken".to_owned(),
            _ => format!("counts {} of its counts in it, below `profile-cold-percent`", hit.percent()),
        };
        let span = cx.span(range);
        cx.blaze(
            Lint::StaleHotMarker,
            at,
            span,
            format!("`fn {name}` is marked hot (`{marker}`), and profile `{file}` {counted}"),
        )
        .note(format!(
            "the profile is `{}`, named in {}",
            loaded.settings.path.display(),
            loaded.settings.named_in
        ))
        .help(Lint::StaleHotMarker.help())
        .help(format!("for further information visit {}", Lint::StaleHotMarker.docs_url()));
    }
    if cx.on_blaze(Lint::HotMarkedCold)
        && hit.hot
        && AttrFlags::query(db, AttrDefId::FunctionId(id)).contains(AttrFlags::COLD)
    {
        let span = cx.span(range);
        cx.blaze(
            Lint::HotMarkedCold,
            at,
            span,
            format!(
                "`fn {name}` is `#[cold]`, and profile `{file}` counts {} of its counts in it, at or above `profile-hot-percent`",
                hit.percent()
            ),
        )
        .note(format!("the profile is `{}`, named in {}", loaded.settings.path.display(), loaded.settings.named_in))
        .help(Lint::HotMarkedCold.help())
        .help(format!("for further information visit {}", Lint::HotMarkedCold.docs_url()));
    }
}
