//! The context meter's popover (scene/widgets/context.flecs `hn.ChatContext*`). While the pointer is on the meter
//! (pick.rs) or on the popover itself, and the picker's menu is closed, a plate over the meter says what the chat's last
//! `context` event (harness-proto `ContextUse`) holds: a head with the total, the window as a bar of the parts, a row a
//! part (its splits under it) and a foot with the last call's cache split and whose numbers these are. Each line is a
//! template instance whose props this sets; the look is all scene data.
use super::pick::tokens;
use super::*;
use harness_proto::event::{ContextCall, ContextPart, ContextUse};
use std::fmt::Write as _;

/// The scene's ink slots for the parts of the whole, in order (hn.ChatContextInk); a part past the last takes the first
/// again.
const INKS: [&str; 8] = ["s1", "s2", "s3", "s4", "s5", "s6", "s7", "s8"];
/// hn.ChatContextInk's slot for a [`ContextPart::REST`] part.
const REST_INK: &str = "rest";

/// The popover in `pop` (the status line's overlay slot for it) over `meter` (its hn.ChatContext), for context `c` as an
/// agent of kind `agent` reported it; none while `hidden` (the picker's menu is open) or while the pointer is elsewhere.
pub(super) fn fill_context_pop(pop: View<'_>, meter: Option<View<'_>>, c: Option<&ContextUse>, agent: &str, hidden: bool) {
    let w = pop.entity().world();
    let on = |m: View<'_>| {
        let (m, p) = (m.id(), pop.id());
        let hovered = w.entity_named(APP).target(Hovered::id(), 0);
        hovered.is_some_and(|h| std::iter::successors(Some(h), |n| n.parent()).take(64).any(|n| *n.id() == m || *n.id() == p))
    };
    let shown = match (meter, c) {
        (Some(m), Some(c)) if !hidden && on(m) => Some((m, c)),
        _ => None,
    };
    view::for_each_of(pop, shown.map(|_| ("plate", "hn.ChatContextPop", ())), |_, (), _| {});
    if let (Some(plate), Some((meter, c))) = (pop.part("plate"), shown) {
        plate.anchored(meter.id());
        lines(plate, c, agent);
    }
}

/// One line of the plate.
enum Line<'c> {
    Head,
    Bar,
    Part(&'c ContextPart, &'static str),
    Foot,
}

/// The plate's lines for `c`.
fn lines(plate: View<'_>, c: &ContextUse, agent: &str) {
    let mut slot = 0;
    let inked = c.parts.iter().map(|p| {
        let ink = if !p.of.is_empty() {
            ""
        } else if p.estimate == ContextPart::REST {
            REST_INK
        } else {
            slot += 1;
            INKS[(slot - 1) % INKS.len()]
        };
        (p, ink)
    });
    // alloc: a dozen parts' keys, while the popover is up
    let parts: Vec<(String, &str, Line<'_>)> = inked.map(|(p, ink)| (format!("p.{}", p.id), "hn.ChatContextRow", Line::Part(p, ink))).collect();
    let rows = [("head".to_owned(), "hn.ChatContextHead", Line::Head), ("bar".to_owned(), "hn.ChatContextBar", Line::Bar)] // alloc: two keys
        .into_iter()
        .chain(parts)
        .chain([("foot".to_owned(), "hn.ChatContextFoot", Line::Foot)]); // alloc: one key
    view::for_each_of(plate, rows, |v, line, _| match line {
        Line::Head => {
            let (used, window, pct) = (tokens(c.used), tokens(c.window), share(c.used, c.window));
            v.props("hn.ChatContextHead", &[("used", Prop::Str(&used)), ("window", Prop::Str(&window)), ("pct", Prop::Str(&pct))]);
        }
        Line::Bar => {
            if let Some(track) = v.part("track") {
                segments(track, c);
            }
        }
        Line::Part(p, ink) => {
            let split = !p.of.is_empty();
            let (n, s) = (tokens(p.tokens), if split { String::new() } else { share(p.tokens, c.window) });
            v.props(
                "hn.ChatContextRow",
                &[
                    ("name", Prop::Str(&p.name)),
                    ("tokens", Prop::Str(&n)),
                    ("share", Prop::Str(&s)),
                    ("ink", Prop::Str(ink)),
                    ("split", Prop::Bool(split)),
                    ("estimate", Prop::Bool(!p.estimate.is_empty())),
                ],
            );
        }
        Line::Foot => {
            let cache = c.last.as_ref().map(cache_line).unwrap_or_default();
            let how = how_line(c, agent);
            v.props("hn.ChatContextFoot", &[("cache", Prop::Str(&cache)), ("how", Prop::Str(&how))]);
        }
    });
}

/// The bar's segments on `track`: one for each part of the whole that holds tokens, its share of the window, in its
/// row's ink. Laid end to end they never pass the track's end (claude's own counts can sum a little past its total).
fn segments(track: View<'_>, c: &ContextUse) {
    let mut slot = 0;
    let mut left = 1.0_f64;
    let segs = c.parts.iter().filter(|p| p.of.is_empty()).filter_map(|p| {
        let ink = if p.estimate == ContextPart::REST {
            REST_INK
        } else {
            slot += 1;
            INKS[(slot - 1) % INKS.len()]
        };
        let f = (p.tokens as f64 / c.window as f64).min(left);
        left -= f;
        (p.tokens > 0).then(|| (format!("seg.{}", p.id), "hn.ChatContextSeg", (f, ink))) // alloc: a key a part
    });
    view::for_each_of(track, segs, |v, (f, ink), _| {
        v.props("hn.ChatContextSeg", &[("share", Prop::Float(f)), ("ink", Prop::Str(ink))]);
    });
}

/// `n` as a share of `of`: `12%`, `<1%` for a part that holds less than half a percent.
fn share(n: u64, of: u64) -> String {
    let f = n as f64 * 100.0 / of.max(1) as f64;
    if n > 0 && f < 0.5 { "<1%".to_owned() } else { format!("{:.0}%", f.min(100.0)) }
}

/// The last call's input as the cache split it, then its output: `Last call: 30k cached · 1.3k written · 2 uncached ·
/// 10 out · 0 thinking` (no `written` when it wrote nothing; no `thinking` when the agent did not say).
fn cache_line(l: &ContextCall) -> String {
    let mut s = format!("Last call: {} cached", tokens(l.cache_read)); // alloc: one line, while the popover is up
    if l.cache_write > 0 {
        let _ = write!(s, " · {} written", tokens(l.cache_write));
    }
    let _ = write!(s, " · {} uncached · {} out", tokens(l.input), tokens(l.output));
    if let Some(t) = l.thinking {
        let _ = write!(s, " · {} thinking", tokens(t));
    }
    s
}

/// Whose numbers these are: the agent's (`agent`), or harness's estimates (the rows marked `≈`).
fn how_line(c: &ContextUse, agent: &str) -> String {
    let who = match agent {
        "claude" => "Claude",
        "codex" => "Codex",
        "cursor" => "Cursor",
        other => other,
    };
    let est = |e: &str| c.parts.iter().any(|p| p.estimate == e);
    if c.parts.is_empty() {
        format!("{who} reported the total only.")
    } else if est(ContextPart::REST) {
        format!("Total from {who}; ≈ counted by harness ({}), the rest is the total less those.", ContextPart::O200K)
    } else if est(ContextPart::O200K) {
        format!("Counts from {who}; ≈ estimated by harness ({}).", ContextPart::O200K)
    } else {
        format!("Counts from {who}.")
    }
}
