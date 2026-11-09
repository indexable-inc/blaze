//! Tier 3's layout and folding (after den's 33-genui-prims.flecs and genui.rs): Grid, Stack, Layer, Wrap, Scroll and Split
//! place their children; Tabs, Disclosure and Accordion show some of them, a press choosing which. Each is its
//! scene/widgets/genui.flecs `hn.Gen<type>`; what a press sends is in its Action, as a control's is (genui/control.rs).
use super::*;
use control::{arm, same, send, sends};
use harness_native_ui_layout_api::{Align, GridColumns, Justify};
use harness_native_ui_node_api::Z;

/// cod-chevron-right (shut), cod-chevron-down (open).
const SHUT: &str = "\u{EAB6}";
const OPEN: &str = "\u{EAB4}";

/// Draw tier-3 layout element `node` (of `kind`, sitting in `within`) on `v`, with its children `kids`.
pub(super) fn element(v: View<'_>, node: &Value, kind: &str, kids: &[&Value], depth: usize, within: &Within, b: &mut Budget<'_, '_>) {
    let cell = b.cell;
    match kind {
        "Grid" => grid(v, node, kids, depth, b),
        "Stack" => {
            // Where its children that are not Layers sit (a Layer has its own anchor): `align`, else the middle.
            let (across, down) = anchored(prop(node, "align").as_str().unwrap_or("center"));
            let base = view::prefab(v.entity().world(), "hn.GenStack").and_then(|p| p.try_cloned::<&Flex>()).unwrap_or_default();
            let flex = Flex { align: across.0, justify: down.1, ..base };
            if v.entity().try_cloned::<&Flex>() != Some(flex) {
                v.set(flex);
            }
            b.next = Within::Stack;
            children(v, kids, depth + 1, b);
        }
        "Layer" => layer(v, node, kids, depth, b),
        "Wrap" => {
            spacing(v, "hn.GenWrap", node);
            children(v, kids, depth + 1, b);
        }
        "Scroll" => {
            let h = points(node, "height").unwrap_or(240.0).min(props::MAX_SCROLL_H as f32);
            let mut s = v.entity().try_cloned::<&Size>().unwrap_or_default();
            if s.h != Sizing::fixed(h) {
                s.h = Sizing::fixed(h);
                v.set(s);
            }
            children(v, kids, depth + 1, b);
        }
        "Split" => control::split(v, node, kids, depth, b),
        "Tabs" => {
            let tabs = props::tabs(node).unwrap_or_default();
            let sel = match prop(node, "selected") {
                Value::Null => tabs.first().map_or(Value::Null, |t| t.1.clone()),
                s => s.clone(),
            };
            let h = control::handler(node, "onChange");
            if let Some(row) = v.part("bar").and_then(|bar| bar.part("tabs")) {
                let rows = tabs.iter().enumerate().map(|(i, t)| (format!("t{i}"), if same(&sel, &t.1) { "hn.GenTabOn" } else { "hn.GenTab" }, t));
                view::for_each_of(row, rows, |tab, (label, value), _| {
                    if let Some(l) = tab.part("head").and_then(|hd| hd.part("label")) {
                        l.text(label);
                    }
                    arm(tab, h.as_deref().map(|h| send(cell, h, value.clone())).as_deref());
                });
            }
            if let Some(body) = v.part("body") {
                children(body, kids, depth + 1, b);
            }
        }
        "Disclosure" => {
            let open = prop(node, "open") == &Value::Bool(true);
            let h = control::handler(node, "onToggle");
            let parts: &[(&str, &str)] =
                if open { &[("head", "hn.GenDisclosureHead"), ("body", "hn.GenDisclosureBody")] } else { &[("head", "hn.GenDisclosureHead")] };
            view::for_each_of(v, parts.iter().map(|(k, p)| (*k, *p, ())), |_, (), _| {});
            if let Some(head) = v.part("head") {
                if let Some(c) = head.part("chevron") {
                    c.icon(if open { OPEN } else { SHUT });
                }
                if let Some(t) = head.part("title") {
                    t.text(&words(node, "title"));
                }
                // In an Accordion, opening this one closes the one open first.
                let events: Vec<(String, Value)> = match (&h, within) {
                    (Some(h), Within::Accordion(Some(o))) if !open && o != h => vec![(o.clone(), Value::Bool(false)), (h.clone(), Value::Bool(true))],
                    (Some(h), _) => vec![(h.clone(), Value::Bool(!open))],
                    (None, _) => Vec::new(),
                };
                arm(head, (!events.is_empty()).then(|| sends(cell, &events)).as_deref());
            }
            if let Some(body) = v.part("body") {
                children(body, kids, depth + 1, b);
            }
        }
        "Accordion" => {
            let open = kids.iter().find(|k| k["props"]["open"] == true).and_then(|k| k["props"]["onToggle"].as_str()).map(str::to_owned);
            b.next = Within::Accordion(open);
            children(v, kids, depth + 1, b);
        }
        _ => {}
    }
}

/// A Grid: `columns` equal columns, its children row by row in rows of cells (the last row's spare cells keep their
/// width), `gap` apart both ways; or as many columns at least `fit` wide as fit (`hn.GenGridFit`).
fn grid(v: View<'_>, node: &Value, kids: &[&Value], depth: usize, b: &mut Budget<'_, '_>) {
    if let Some(fit) = points(node, "fit") {
        spacing(v, "hn.GenGridFit", node);
        let want = GridColumns { min: fit, max: 0.0 };
        if v.entity().try_cloned::<&GridColumns>() != Some(want) {
            v.set(want);
        }
        children(v, kids, depth + 1, b);
        return;
    }
    spacing(v, "hn.GenGrid", node);
    let n = prop(node, "columns").as_u64().unwrap_or(2).clamp(1, 12) as usize;
    let gap = v.entity().try_cloned::<&Flex>().map_or(0.0, |f| f.gap);
    let base = view::prefab(v.entity().world(), "hn.GenGridRow").and_then(|p| p.try_cloned::<&Flex>()).unwrap_or_default();
    let rows = kids.chunks(n).enumerate().map(|(i, r)| (format!("r{i}"), "hn.GenGridRow", r));
    view::for_each_of(v, rows, |row, r, _| {
        let want = Flex { gap, ..base };
        if row.entity().try_cloned::<&Flex>() != Some(want) {
            row.set(want);
        }
        let cells = (0..n).map(|j| (format!("c{j}"), "hn.GenGridCell", r.get(j).copied()));
        view::for_each_of(row, cells, |c, kid, _| children(c, kid.as_slice(), depth + 1, b));
    });
}

/// Where anchor `anchor` (one of props.rs's ANCHORS) sits, across and then down, each as an alignment and a justification.
fn anchored(anchor: &str) -> ((Align, Justify), (Align, Justify)) {
    let (down, across) = anchor.split_once('-').unwrap_or(match anchor {
        "top" | "bottom" => (anchor, "center"),
        _ => ("center", anchor),
    });
    let at = |s: &str| match s {
        "top" | "left" => (Align::Start, Justify::Start),
        "bottom" | "right" => (Align::End, Justify::End),
        _ => (Align::Center, Justify::Center),
    };
    (at(across), at(down))
}

/// A Layer in a Stack: its content's size towards the stack's (a Stack is as big as its largest child, Layers included),
/// stretched over the stack's whole frame (`hn.GenLayer`'s AlignSelf), its children at its `anchor` (the column's
/// alignment across and down) moved by `x` and `y` (its padding: what it gives on one side it takes on the other, so
/// the offset adds no size), above its siblings by `z`. A press on it sends its onClick (null).
fn layer(v: View<'_>, node: &Value, kids: &[&Value], depth: usize, b: &mut Budget<'_, '_>) {
    let (across, down) = anchored(prop(node, "anchor").as_str().unwrap_or("top-left"));
    let base = view::prefab(v.entity().world(), "hn.GenLayer").and_then(|p| p.try_cloned::<&Flex>()).unwrap_or_default();
    let flex = Flex { align: across.0, justify: down.1, ..base };
    if v.entity().try_cloned::<&Flex>() != Some(flex) {
        v.set(flex);
    }
    let num = |n: &str| prop(node, n).as_f64().filter(|x| x.is_finite()).unwrap_or(0.0) as f32;
    let (x, y) = (num("x"), num("y"));
    let pad = Padding { l: x, r: -x, t: y, b: -y };
    if v.entity().try_cloned::<&Padding>() != Some(pad) {
        v.set(pad);
    }
    let z = Z { z: num("z") };
    if v.entity().try_cloned::<&Z>() != Some(z) {
        v.set(z);
    }
    arm(v, control::handler(node, "onClick").map(|h| send(b.cell, &h, Value::Null)).as_deref());
    children(v, kids, depth + 1, b);
}
