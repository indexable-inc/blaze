//! The context meter's popover (scene/widgets/context.flecs `hn.ChatContext*`). While the pointer is on the meter
//! (pick.rs) or on the popover itself, and the picker's menu is closed, a plate over the meter shows what the chat's last
//! `context` event (harness-proto `ContextUse`) holds at a glance: a head with the total and the window as a donut of
//! its parts; the pointer on a slice names that part in the donut's middle, the others dimmed. A press on the meter
//! opens it (`open`, until another press or Escape) to the whole account: a row a part (its splits under it) and a foot
//! with the last call's cache split and whose numbers these are. Each line is a template instance whose props this sets;
//! the look is all scene data.
use super::pick::tokens;
use super::*;
use harness_proto::event::{ContextCall, ContextPart, ContextUse};
use std::fmt::Write as _;

/// The scene's ink slots for the parts of the whole, in order (hn.ChatContextInk); a part past the last takes the first
/// again.
const INKS: [&str; 8] = ["s1", "s2", "s3", "s4", "s5", "s6", "s7", "s8"];
/// hn.ChatContextInk's slot for a [`ContextPart::REST`] part.
const REST_INK: &str = "rest";

/// The popover in `pop` (the status bar's overlay slot for it) over `meter` (its hn.ChatContext ring), for context `c` as an
/// agent of kind `agent` reported it; none while `hidden` (the picker's menu is open) or while the pointer is elsewhere,
/// unless it was pressed `open`. `pointer`: where the pointer is (view points), for the slice under it.
pub(super) fn fill_context_pop(
    pop: View<'_>, meter: Option<View<'_>>, c: Option<&ContextUse>, agent: &str, hidden: bool, open: bool, pointer: (f32, f32),
) {
    let w = pop.entity().world();
    let on = |m: View<'_>| {
        let (m, p) = (m.id(), pop.id());
        let hovered = w.entity_named(APP).target(Hovered::id(), 0);
        hovered.is_some_and(|h| std::iter::successors(Some(h), |n| n.parent()).take(64).any(|n| *n.id() == m || *n.id() == p))
            // Open, it stays open while the pointer crosses the gap from the ring onto its window.
            || pop.part("plate").is_some_and(|plate| view::bridged(w.entity_from_id(m), plate.entity()))
    };
    let shown = match (meter, c) {
        (Some(m), Some(c)) if !hidden && (open || on(m)) => Some((m, c)),
        _ => None,
    };
    view::for_each_of(pop, shown.map(|_| ("plate", "hn.ChatContextPop", ())), |_, (), _| {});
    if let (Some(plate), Some((meter, c))) = (pop.part("plate"), shown) {
        plate.anchored(meter.id());
        lines(plate, c, agent, open, pointer);
    }
}

/// One line of the plate.
enum Line<'c> {
    Head,
    Bar,
    Part(&'c ContextPart, &'static str),
    Foot,
}

/// The plate's lines for `c`: the head and the donut, and opened (`open`) a row a part and the foot.
fn lines(plate: View<'_>, c: &ContextUse, agent: &str, open: bool, pointer: (f32, f32)) {
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
    let parts: Vec<(String, &str, Line<'_>)> =
        if open { inked.map(|(p, ink)| (format!("p.{}", p.id), "hn.ChatContextRow", Line::Part(p, ink))).collect() } else { Vec::new() };
    let foot = open.then(|| ("foot".to_owned(), "hn.ChatContextFoot", Line::Foot)); // alloc: one key
    let rows = [("head".to_owned(), "hn.ChatContextHead", Line::Head), ("pie".to_owned(), "hn.ChatContextPie", Line::Bar)] // alloc: two keys
        .into_iter()
        .chain(parts)
        .chain(foot);
    view::for_each_of(plate, rows, |v, line, _| match line {
        Line::Head => {
            let (used, window, pct) = (tokens(c.used), tokens(c.window), share(c.used, c.window));
            v.props("hn.ChatContextHead", &[("used", Prop::Str(&used)), ("window", Prop::Str(&window)), ("pct", Prop::Str(&pct))]);
        }
        Line::Bar => {
            if let Some(chart) = v.part("chart") {
                let slices = slices_of(c);
                let under = under_pointer(chart, &slices, pointer);
                // In its middle: how full the window is, or the part under the pointer and its share.
                let (big, small) = match under.and_then(|i| slices.get(i)) {
                    Some(s) => (share(s.part.tokens, c.window), s.part.name.clone()),
                    None => (share(c.used, c.window), "used".to_owned()),
                };
                if let Some(mid) = chart.part("mid") {
                    if let Some(p) = mid.part("pct") {
                        p.text(&big);
                    }
                    if let Some(l) = mid.part("label") {
                        l.text(&small);
                    }
                }
                if let Some(s) = chart.part("slices") {
                    pie(s, &slices, under);
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

/// One slice of the donut: its part, its ink slot, and where it runs (shares of a turn, clockwise from the top).
struct Slice<'c> {
    part: &'c ContextPart,
    ink: &'static str,
    from: f64,
    to: f64,
}

/// The donut's slices for `c`: one for each part of the whole that holds tokens, its share of the window after the ones
/// before it, in its row's ink. They never pass the ring's end (claude's own counts can sum a little past its total).
fn slices_of(c: &ContextUse) -> Vec<Slice<'_>> {
    let mut slot = 0;
    let mut at = 0.0_f64;
    let mut out = Vec::new(); // alloc: a dozen slices, while the popover is up
    for p in c.parts.iter().filter(|p| p.of.is_empty()) {
        let ink = if p.estimate == ContextPart::REST {
            REST_INK
        } else {
            slot += 1;
            INKS[(slot - 1) % INKS.len()]
        };
        let f = (p.tokens as f64 / c.window as f64).min(1.0 - at);
        if p.tokens > 0 && f > 0.0 {
            out.push(Slice { part: p, ink, from: at, to: at + f });
        }
        at += f.max(0.0);
    }
    out
}

/// Which of `slices` the pointer is on in `chart` (the donut's square, its ring between 34 and 48 of 50 out from the
/// middle, as [`ring_path`] draws it): by its distance from the middle and its angle clockwise from the top.
fn under_pointer(chart: View<'_>, slices: &[Slice<'_>], (x, y): (f32, f32)) -> Option<usize> {
    let l = chart.entity().try_cloned::<&Laid>()?;
    let (cx, cy, half) = (l.x + l.w / 2.0, l.y + l.h / 2.0, l.w.min(l.h) / 2.0);
    let (dx, dy) = (f64::from(x - cx), f64::from(y - cy));
    let r = dx.hypot(dy) / f64::from(half.max(1.0)) * 50.0;
    if !(30.0..=50.0).contains(&r) {
        return None;
    }
    let turn = dx.atan2(-dy).rem_euclid(std::f64::consts::TAU) / std::f64::consts::TAU;
    slices.iter().position(|s| (s.from..s.to).contains(&turn))
}

/// The donut's slices in `slices`, each giving up a hair at its end so neighbours read apart; with one under the pointer
/// (`under`), the others dimmed.
fn pie(slices: View<'_>, all: &[Slice<'_>], under: Option<usize>) {
    let parts = all.iter().enumerate().map(|(i, s)| {
        let d = ring_path(s.from, s.to - (s.to - s.from).min(SLICE_GAP) * 0.5);
        (format!("slice.{}", s.part.id), "hn.ChatContextSlice", (d, s.ink, under.is_some_and(|u| u != i))) // alloc: a key a part
    });
    view::for_each_of(slices, parts, |v, (d, ink, dim), _| {
        v.props("hn.ChatContextSlice", &[("d", Prop::Str(&d)), ("ink", Prop::Str(ink)), ("dim", Prop::Bool(dim))]);
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
