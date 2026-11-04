//! The flex solver, pure over an index tree so it needs no world (docs/ui-model.md section 4, the Clay model).
//!
//! Passes, per axis `a` (0 width, 1 height): `natural(a)` bottom-up content sizes, `assign(a)` top-down sizes (Fixed,
//! Percent, Fit, Grow by weight with min and max, shrink down to min). Width runs first, then height, then `place` writes
//! positions. A leaf's content size is its `Intrinsic` (a component the host's typesetter writes, `typeset`).
use harness_native_ui_layout_api::{Align, FlexAxis, Flex, Justify, Opens, SizeKind, Sizing};
use harness_native_ui_node_api::Laid;

/// One node as the solver sees it.
#[derive(Clone, Debug, Default)]
pub struct Item {
    pub id: u64,
    pub size: [Sizing; 2],
    pub flex: Flex,
    /// Left, right, top, bottom.
    pub pad: [f32; 4],
    pub aspect: f32,
    /// A leaf's content size: a `Text` unwrapped width and its height at the width it was measured at.
    pub intrinsic: [f32; 2],
    pub scroll: [f32; 2],
    /// Its `Centre`: where along the main axis its children's middle goes, as a share of its length.
    pub centre: Option<f32>,
    /// `ShrinkFirst`: out of room, it gives up its length before its siblings.
    pub shrink_first: bool,
    pub children: Vec<usize>,
    pub nat: [f32; 2],
    pub out: Laid,
}

impl Item {
    fn dim(&self, a: usize) -> f32 {
        if a == 0 { self.out.w } else { self.out.h }
    }

    fn set_dim(&mut self, a: usize, v: f32) {
        if a == 0 { self.out.w = v } else { self.out.h = v }
    }

    fn pads(&self, a: usize) -> f32 {
        self.pad[a * 2] + self.pad[a * 2 + 1]
    }

    fn main(&self, a: usize) -> bool {
        matches!((self.flex.axis, a), (FlexAxis::Row, 0) | (FlexAxis::Column, 1))
    }

    fn wrapping(&self) -> bool {
        self.flex.wrap && self.flex.axis == FlexAxis::Row
    }
}

/// Lay out the tree under `root` into `items[..].out`. `root_room` is the room the root may fill (the viewport).
pub fn solve(items: &mut [Item], root: usize, root_room: [f32; 2]) {
    natural(items, root, 0);
    let w = root_size(&items[root], 0, root_room[0]);
    assign(items, root, 0, w);
    natural(items, root, 1);
    let h = root_size(&items[root], 1, root_room[1]);
    assign(items, root, 1, h);
    place(items, root);
}

fn root_size(it: &Item, a: usize, room: f32) -> f32 {
    let s = it.size[a];
    match s.kind {
        SizeKind::Fixed => s.value,
        SizeKind::Percent => s.clamp(s.value * room),
        SizeKind::Grow => s.clamp(room),
        SizeKind::Fit => it.nat[a],
    }
}

/// What a child adds to its parent's fit size on axis `a`: Percent adds nothing (it is a share of an unknown).
fn contribution(it: &Item, a: usize) -> f32 {
    if it.size[a].kind == SizeKind::Percent { 0.0 } else { it.nat[a] }
}

fn natural(items: &mut [Item], i: usize, a: usize) {
    let kids = items[i].children.clone();
    for &c in &kids {
        natural(items, c, a);
    }
    let it = &items[i];
    let content = if kids.is_empty() {
        it.intrinsic[a]
    } else if it.flex.axis == FlexAxis::Z {
        kids.iter().map(|&c| contribution(&items[c], a)).fold(0.0, f32::max)
    } else if it.main(a) {
        let sum: f32 = kids.iter().map(|&c| contribution(&items[c], a)).sum();
        sum + it.flex.gap * (kids.len() - 1) as f32
    } else if it.wrapping() {
        let lines = lines(items, i);
        let heights: f32 = lines.iter().map(|l| l.iter().map(|&c| items[c].nat[1]).fold(0.0, f32::max)).sum();
        heights + it.flex.gap * lines.len().saturating_sub(1) as f32
    } else {
        kids.iter().map(|&c| contribution(&items[c], a)).fold(0.0, f32::max)
    };
    let it = &mut items[i];
    let mut n = content + it.pads(a);
    if a == 0 && it.aspect > 0.0 && it.size[0].kind == SizeKind::Fit && it.size[1].kind == SizeKind::Fixed {
        n = it.size[1].value * it.aspect;
    }
    if a == 1 && it.aspect > 0.0 && it.size[1].kind == SizeKind::Fit {
        n = it.out.w / it.aspect;
    }
    it.nat[a] = if it.size[a].kind == SizeKind::Fixed { it.size[a].value } else { it.size[a].clamp(n) };
}

/// Break a wrapping Row's children into lines by their assigned widths.
fn lines(items: &[Item], i: usize) -> Vec<Vec<usize>> {
    let it = &items[i];
    let inner = (it.out.w - it.pads(0)).max(0.0);
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut used = 0.0;
    for &c in &it.children {
        let w = items[c].out.w;
        match out.last_mut() {
            Some(line) if used + it.flex.gap + w <= inner + 0.01 => {
                line.push(c);
                used += it.flex.gap + w;
            }
            _ => {
                out.push(vec![c]);
                used = w;
            }
        }
    }
    out
}

fn assign(items: &mut [Item], i: usize, a: usize, size: f32) {
    items[i].set_dim(a, size);
    let it = &items[i];
    let kids = it.children.clone();
    if kids.is_empty() {
        return;
    }
    let inner = (size - it.pads(a)).max(0.0);
    let sizes: Vec<f32> = if it.main(a) && !(it.wrapping() && a == 0) {
        distribute(items, i, a, inner)
    } else {
        kids.iter()
            .map(|&c| {
                let ch = &items[c];
                let s = ch.size[a];
                match s.kind {
                    SizeKind::Fixed => s.value,
                    SizeKind::Percent => s.clamp(s.value * inner),
                    SizeKind::Grow if !(it.wrapping() && a == 1) => s.clamp(inner),
                    SizeKind::Fit if it.flex.align == Align::Stretch && it.flex.axis != FlexAxis::Z => s.clamp(inner),
                    _ => ch.nat[a],
                }
            })
            .collect()
    };
    for (&c, s) in kids.iter().zip(sizes) {
        assign(items, c, a, s);
    }
}

/// Main-axis sizes of the children of `i`: bases, then free room to Grow by weight, or a deficit shrunk off the rest.
fn distribute(items: &[Item], i: usize, a: usize, inner: f32) -> Vec<f32> {
    let it = &items[i];
    let kids = &it.children;
    let gaps = it.flex.gap * kids.len().saturating_sub(1) as f32;
    let mut sizes: Vec<f32> = kids
        .iter()
        .map(|&c| {
            let ch = &items[c];
            let s = ch.size[a];
            match s.kind {
                SizeKind::Fixed => s.value,
                SizeKind::Percent => s.clamp(s.value * inner),
                SizeKind::Fit => ch.nat[a],
                SizeKind::Grow => s.min,
            }
        })
        .collect();
    let mut free = inner - gaps - sizes.iter().sum::<f32>();
    if free > 0.0 {
        let mut open: Vec<usize> = (0..kids.len()).filter(|&k| items[kids[k]].size[a].kind == SizeKind::Grow).collect();
        while free > 0.01 && !open.is_empty() {
            let weight = |k: usize| items[kids[k]].size[a].value.max(0.0).max(f32::from(items[kids[k]].size[a].value <= 0.0));
            let total: f32 = open.iter().map(|&k| weight(k)).sum();
            let mut spent = 0.0;
            let mut capped = Vec::new();
            for &k in &open {
                let s = items[kids[k]].size[a];
                let want = sizes[k] + free * weight(k) / total;
                let got = if s.max > 0.0 { want.min(s.max) } else { want };
                if s.max > 0.0 && want >= s.max {
                    capped.push(k);
                }
                spent += got - sizes[k];
                sizes[k] = got;
            }
            free -= spent;
            open.retain(|k| !capped.contains(k));
            if capped.is_empty() {
                break;
            }
        }
    } else if free < -0.01 {
        // `ShrinkFirst` children give up their length first; the rest shrink after, only if that was not enough.
        let can = |k: usize| matches!(items[kids[k]].size[a].kind, SizeKind::Fit | SizeKind::Grow);
        for first in [true, false] {
            let shrinkable: Vec<usize> = (0..kids.len()).filter(|&k| can(k) && (!first || items[kids[k]].shrink_first)).collect();
            free = shrink(items, kids, a, &mut sizes, shrinkable, free);
        }
    }
    sizes
}

/// Shrink the `shrinkable` children of `kids` along axis `a` in proportion to their sizes, each down to its `min`, until
/// `free` (negative: the overflow) is spent. Returns what is left of it.
fn shrink(items: &[Item], kids: &[usize], a: usize, sizes: &mut [f32], mut shrinkable: Vec<usize>, mut free: f32) -> f32 {
    while free < -0.01 && !shrinkable.is_empty() {
        let total: f32 = shrinkable.iter().map(|&k| sizes[k]).sum();
        if total <= 0.0 {
            break;
        }
        let mut floored = Vec::new();
        let deficit = -free;
        for &k in &shrinkable {
            let min = items[kids[k]].size[a].min;
            let want = sizes[k] - deficit * sizes[k] / total;
            if want <= min {
                floored.push(k);
            }
            let got = want.max(min);
            free += sizes[k] - got;
            sizes[k] = got;
        }
        shrinkable.retain(|k| !floored.contains(k));
        if floored.is_empty() {
            break;
        }
    }
    free
}

fn place(items: &mut [Item], i: usize) {
    let (flex, pad, scroll, centre, w, h, kids) = {
        let it = &items[i];
        (it.flex, it.pad, it.scroll, it.centre, it.out.w, it.out.h, it.children.clone())
    };
    if kids.is_empty() {
        return;
    }
    let (ox, oy) = (items[i].out.x + pad[0] - scroll[0], items[i].out.y + pad[2] - scroll[1]);
    let (iw, ih) = ((w - pad[0] - pad[1]).max(0.0), (h - pad[2] - pad[3]).max(0.0));
    let across = |align: Align, room: f32, size: f32| match align {
        Align::Start | Align::Stretch => 0.0,
        Align::Center => (room - size) / 2.0,
        Align::End => room - size,
    };
    match flex.axis {
        FlexAxis::Z => {
            for &c in &kids {
                let (cw, ch) = (items[c].out.w, items[c].out.h);
                items[c].out.x = ox + across(flex.align, iw, cw);
                items[c].out.y = oy + across(flex.align, ih, ch);
            }
        }
        FlexAxis::Row if flex.wrap => {
            let mut y = oy;
            let ls = lines(items, i);
            for line in ls {
                let lh = line.iter().map(|&c| items[c].out.h).fold(0.0, f32::max);
                let mut x = ox;
                for c in line {
                    items[c].out.x = x;
                    items[c].out.y = y + across(flex.align, lh, items[c].out.h);
                    x += items[c].out.w + flex.gap;
                }
                y += lh + flex.gap;
            }
        }
        axis => {
            let a = usize::from(axis == FlexAxis::Column);
            let sizes: Vec<f32> = kids.iter().map(|&c| items[c].dim(a)).collect();
            let used: f32 = sizes.iter().sum::<f32>() + flex.gap * (kids.len() - 1) as f32;
            let room = if a == 0 { iw } else { ih };
            let free = (room - used).max(0.0);
            let lead = if a == 0 { pad[0] } else { pad[2] };
            let (mut at, step) = match (centre, flex.justify) {
                (Some(c), _) => ((c * if a == 0 { w } else { h } - used / 2.0 - lead).clamp(0.0, free), flex.gap),
                (None, Justify::Start) => (0.0, flex.gap),
                (None, Justify::Center) => (free / 2.0, flex.gap),
                (None, Justify::End) => (free, flex.gap),
                (None, Justify::SpaceBetween) if kids.len() > 1 => (0.0, flex.gap + free / (kids.len() - 1) as f32),
                (None, Justify::SpaceBetween) => (0.0, flex.gap),
            };
            let cross_room = if a == 0 { ih } else { iw };
            for (&c, s) in kids.iter().zip(sizes) {
                let cross = across(flex.align, cross_room, items[c].dim(1 - a));
                if a == 0 {
                    items[c].out.x = ox + at;
                    items[c].out.y = oy + cross;
                } else {
                    items[c].out.x = ox + cross;
                    items[c].out.y = oy + at;
                }
                at += s + step;
            }
        }
    }
    for c in kids {
        place(items, c);
    }
}

/// Where an anchored overlay of `size` goes against `anchor`, flipped to the opposite side when it overflows and then
/// clamped inside `room`.
#[must_use]
pub fn anchored(anchor: Laid, size: [f32; 2], edge: Opens, nudge: [f32; 2], room: [f32; 2]) -> [f32; 2] {
    let at = |e: Opens| match e {
        Opens::Bottom => [anchor.x + nudge[0], anchor.y + anchor.h + nudge[1]],
        Opens::Top => [anchor.x + nudge[0], anchor.y - size[1] - nudge[1]],
        Opens::Right => [anchor.x + anchor.w + nudge[0], anchor.y + nudge[1]],
        Opens::Left => [anchor.x - size[0] - nudge[0], anchor.y + nudge[1]],
    };
    let fits = |p: [f32; 2]| p[0] >= 0.0 && p[1] >= 0.0 && p[0] + size[0] <= room[0] && p[1] + size[1] <= room[1];
    let flipped = match edge {
        Opens::Bottom => Opens::Top,
        Opens::Top => Opens::Bottom,
        Opens::Right => Opens::Left,
        Opens::Left => Opens::Right,
    };
    let mut p = at(edge);
    if !fits(p) && fits(at(flipped)) {
        p = at(flipped);
    }
    [p[0].min(room[0] - size[0]).max(0.0), p[1].min(room[1] - size[1]).max(0.0)]
}

/// Move the subtree under `i` by `(dx, dy)` (an overlay root placed after its own pass).
pub fn shift(items: &mut [Item], i: usize, dx: f32, dy: f32) {
    items[i].out.x += dx;
    items[i].out.y += dy;
    for c in items[i].children.clone() {
        shift(items, c, dx, dy);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_native_ui_layout_api::FlexAxis;

    fn item(size: [Sizing; 2], children: Vec<usize>) -> Item {
        Item { size, children, ..Item::default() }
    }

    /// A row of a fixed box, a spacer and a fixed box, 8 apart in 200 points: the spacer takes what is left.
    #[test]
    fn a_grow_child_takes_the_rest_of_the_row() {
        let row = Item { flex: Flex { axis: FlexAxis::Row, gap: 8.0, ..Flex::default() }, ..item([Sizing::GROW, Sizing::FIT], vec![1, 2, 3]) };
        let mut items = vec![
            row,
            item([Sizing::fixed(40.0), Sizing::fixed(10.0)], vec![]),
            item([Sizing::GROW, Sizing::FIT], vec![]),
            item([Sizing::fixed(40.0), Sizing::fixed(10.0)], vec![]),
        ];
        solve(&mut items, 0, [200.0, 100.0]);
        assert_eq!(items[2].out.w, 200.0 - 80.0 - 16.0);
        assert_eq!(items[3].out.x, 160.0);
        assert_eq!(items[0].out.h, 10.0);
    }
}
