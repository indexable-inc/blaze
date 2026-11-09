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

/// The popover in `pop` (the composer bar's overlay slot for it) over `meter` (its hn.ChatContext ring), for context `c` as an
/// agent of kind `agent` reported it; none while `hidden` (the picker's menu is open) or while the pointer is elsewhere.
pub(super) fn fill_context_pop(pop: View<'_>, meter: Option<View<'_>>, c: Option<&ContextUse>, agent: &str, hidden: bool) {
    let w = pop.entity().world();
    let on = |m: View<'_>| {
        let (m, p) = (m.id(), pop.id());
        let hovered = w.entity_named(APP).target(Hovered::id(), 0);
        hovered.is_some_and(|h| std::iter::successors(Some(h), |n| n.parent()).take(64).any(|n| *n.id() == m || *n.id() == p))
            // Open, it stays open while the pointer crosses the gap from the ring onto its window.
            || pop.part("plate").is_some_and(|plate| view::bridged(w.entity_from_id(m), plate.entity()))
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
    let rows = [("head".to_owned(), "hn.ChatContextHead", Line::Head), ("pie".to_owned(), "hn.ChatContextPie", Line::Bar)] // alloc: two keys
        .into_iter()
        .chain(parts)
        .chain([("foot".to_owned(), "hn.ChatContextFoot", Line::Foot)]); // alloc: one key
    view::for_each_of(plate, rows, |v, line, _| match line {
        Line::Head => {
            let (used, window, pct) = (tokens(c.used), tokens(c.window), share(c.used, c.window));
            v.props("hn.ChatContextHead", &[("used", Prop::Str(&used)), ("window", Prop::Str(&window)), ("pct", Prop::Str(&pct))]);
        }
        Line::Bar => {
            if let Some(chart) = v.part("chart") {
                if let Some(pct) = chart.part("pct") {
                    pct.text(&share(c.used, c.window));
                }
                if let Some(slices) = chart.part("slices") {
                    pie(slices, c);
                }
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

/// The donut's slices in `slices`: one for each part of the whole that holds tokens, its share of the window clockwise
/// from the top after the ones before it, in its row's ink. They never pass the ring's end (claude's own counts can sum
/// a little past its total), and each gives up a hair at its end so neighbours read apart.
fn pie(slices: View<'_>, c: &ContextUse) {
    let mut slot = 0;
    let mut at = 0.0_f64;
    let parts = c.parts.iter().filter(|p| p.of.is_empty()).filter_map(|p| {
        let ink = if p.estimate == ContextPart::REST {
            REST_INK
        } else {
            slot += 1;
            INKS[(slot - 1) % INKS.len()]
        };
        let f = (p.tokens as f64 / c.window as f64).min(1.0 - at);
        let from = at;
        at += f;
        (p.tokens > 0 && f > 0.0).then(|| (format!("slice.{}", p.id), "hn.ChatContextSlice", (ring_path(from, at - f.min(SLICE_GAP) * 0.5), ink))) // alloc: a key a part
    });
    view::for_each_of(slices, parts, |v, (d, ink), _| {
        v.props("hn.ChatContextSlice", &[("d", Prop::Str(&d)), ("ink", Prop::Str(ink))]);
    });
}

/// The share of the ring each slice gives up at its end, so neighbours read apart.
const SLICE_GAP: f64 = 0.006;

/// A ring segment from `a` to `b` (shares of a turn, clockwise from the top) in a 100-point square: outside radius 48,
/// inside 34, as hn.ChatContextPie's track. A whole turn is drawn as two halves (one arc cannot close on itself).
fn ring_path(a: f64, b: f64) -> String {
    const R: f64 = 48.0;
    const IN: f64 = 34.0;
    if b - a >= 0.9999 {
        return format!("{} {}", ring_path(0.0, 0.5), ring_path(0.5, 1.0));
    }
    let p = |t: f64, r: f64| {
        let th = t * std::f64::consts::TAU;
        (50.0 + r * th.sin(), 50.0 - r * th.cos())
    };
    let large = u8::from(b - a > 0.5);
    let ((x0, y0), (x1, y1), (x2, y2), (x3, y3)) = (p(a, R), p(b, R), p(b, IN), p(a, IN));
    format!("M{x0:.2} {y0:.2} A{R} {R} 0 {large} 1 {x1:.2} {y1:.2} L{x2:.2} {y2:.2} A{IN} {IN} 0 {large} 0 {x3:.2} {y3:.2} Z")
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
