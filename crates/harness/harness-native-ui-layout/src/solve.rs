//! The flex solver, pure over an index tree so it needs no world (docs/ui-model.md section 4, the Clay model).
//!
//! Passes, per axis `a` (0 width, 1 height): `natural(a)` bottom-up content sizes, `assign(a)` top-down sizes (Fixed,
//! Percent, Fit, Grow by weight with min and max, shrink down to min, lowest `LayoutPriority` first). Width runs first,
//! then height, then `place` writes positions. A leaf's content size is its `Intrinsic` (a component the host's
//! typesetter writes, `typeset`).
//!
//! Widths are proposals, SwiftUI's model: a parent proposes the room it has and each child picks its own width within it.
//! Along a row the children share the room (`distribute`: Fit at its ideal, Grow by weight, a deficit shrunk off the
//! lowest priority down to each floor); across a column (and in a Z stack) a child that does not stretch takes the less of
//! its ideal width and the room (`across_size`), so a Fit box around words (a chat bubble, a chip, a card) is as wide as
//! its words up to the room and no wider, and its words wrap at what it has inside. `Size.w.max` is SwiftUI's
//! `.frame(maxWidth:)`, `Size.w.min` the floor a proposal stops at, `FixedSize` is `.fixedSize()` (its ideal whatever is
//! proposed). Before 2026-11-04 a non-stretched child across a column took its ideal width whatever the room, so a long
//! line in a Fit column ran out past its parent (and its plate) on one line.
//!
//! Heights follow widths: a text leaf is as tall as its lines wrapped at the width the width pass gave it. The solver cannot
//! shape text (the layout module is hot-swapped and has no typesetter), so the host measures each `Text` at its last laid
//! width (`typeset`: `Intrinsic.at`), and the frame alternates typeset and layout until no height moves. Widths never
//! depend on heights (but for `Aspect`), so a layout whose widths moved settles one round later, inside the same frame.
//!
//! On top of flex, SwiftUI's layout vocabulary: `Background`/`Overlay` children take their parent's frame out of the flow;
//! a `Grid` sizes columns by their widest cell; `GridColumns` makes adaptive columns; `Lazy` stands for rows that do not
//! exist yet and says which are in view (`range`); a Row aligned on `FirstBaseline`/`LastBaseline` lines up its text;
//! `LaysAtRest` lays a node's children in the room it gets with every moving `RestSize` at that length (`rest_room`,
//! carried down from the moving node's parent) and places them in the frame it has now: what cannot follow a moving box (a
//! terminal's grid, which resizes its pty once per fold). Words follow it: a text breaks its lines at the width it has
//! this frame, so prose re-wraps live as a fold moves (the host only breaks it again over its cached shaping, harness-native-render's
//! `Layout::wrap`); `ViewThatFits` shows the first variant that fits; `SafeAreaPadding` pads a node out of the root's
//! `SafeAreaInset` (a second pass, once the first has said where the node is); a `GrowLimit` caps a growing height at a
//! share of its pane's (`grow_limits`, a second height pass once the first has said how tall the pane is).
use harness_native_ui_layout_api::{Align, FlexAxis, Flex, Justify, Opens, Side, SizeKind, Sizing};
use harness_native_ui_node_api::Laid;

/// Whether a child is in its parent's flow or laid over the parent's whole frame, behind or in front.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Role {
    #[default]
    Flow,
    Background,
    Overlay,
}

/// A `Grid`'s gaps, alignments and its `GridColumn`s (align, width) in order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GridSpec {
    pub hgap: f32,
    pub vgap: f32,
    pub align: Align,
    pub valign: Align,
    pub cols: Vec<(Align, Sizing)>,
}

/// A `Lazy` container's rows: how many, each how long along the main axis, and how many past the view to keep.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LazySpec {
    pub count: u32,
    pub row: f32,
    pub overscan: u32,
}

/// A `GrowLimit`: its height at most `share` of its nearest ancestor's whose height does not come from its content, less
/// `keep`; `max` is its own `Size.h.max` (0: none), which still caps it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LimitSpec {
    pub share: f32,
    pub keep: f32,
    pub max: f32,
}

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
    /// A text leaf's first and last baselines from its text's top (0: none).
    pub baseline: [f32; 2],
    pub scroll: [f32; 2],
    /// It scrolls (`ScrollState`): its content keeps its natural length along its main axis instead of shrinking to fit.
    pub scrolls: bool,
    /// Its `Centre`: where along the main axis its children's middle goes, as a share of its length.
    pub centre: Option<f32>,
    pub align_self: Option<Align>,
    /// Its `AlignmentGuide`: how far its alignment point moves inside it.
    pub guide: [f32; 2],
    pub priority: f32,
    /// `FixedSize`: horizontal, vertical.
    pub fixed: [bool; 2],
    pub role: Role,
    pub grid: Option<GridSpec>,
    /// A `GridRow` (a `Grid` child that is not one spans every column).
    pub grid_row: bool,
    /// `GridColumns` min and max.
    pub adaptive: Option<[f32; 2]>,
    pub lazy: Option<LazySpec>,
    pub lazy_index: Option<u32>,
    /// `ViewThatFits`.
    pub fits: bool,
    /// It clips its children (`Clip`): the view a `Lazy` under it counts rows in.
    pub clip: bool,
    /// Its `SafeAreaInset`, left, right, top, bottom.
    pub safe: [f32; 4],
    /// Its `SafeAreaPadding` edges, left, right, top, bottom.
    pub safe_pad: [bool; 4],
    /// Its `GrowLimit` (`grow_limits` writes the cap it gives into `size[1].max`).
    pub limit: Option<LimitSpec>,
    /// Its `RestSize`: the length its neighbours lay out against while its size along its parent's main axis moves.
    pub rest: Option<f32>,
    /// `LaysAtRest`: its flow children are laid in `rest_room` where it has one, and placed in the frame it has now.
    pub at_rest: bool,
    /// The size it gets on each axis once every moving `RestSize` (a sibling's or an ancestor's sibling's) is at its
    /// `RestSize` (`assign` sets it on every node each pass; None: the size it has now).
    pub rest_room: [Option<f32>; 2],
    pub children: Vec<usize>,
    pub nat: [f32; 2],
    /// How far a parent's shortfall may shrink it on each axis.
    pub floor: [f32; 2],
    /// Its first and last baselines from its top at its natural size (`None`: it has none, it lines up by its bottom).
    pub bl: [Option<f32>; 2],
    /// `ViewThatFits`: which of its flow children shows.
    pub chosen: usize,
    /// A `Grid`'s column widths; `GridColumns`' columns (each as wide).
    pub cols: Vec<f32>,
    pub out: Laid,
    /// Left out by its `ViewThatFits` parent this pass.
    pub hidden: bool,
    /// A `Lazy` container's rows in view, `first..last`.
    pub range: Option<(u32, u32)>,
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

    fn main_axis(&self) -> usize {
        usize::from(self.flex.axis != FlexAxis::Row)
    }

    fn wrapping(&self) -> bool {
        self.flex.wrap && self.flex.axis == FlexAxis::Row
    }

    fn baseline_row(&self) -> Option<usize> {
        match (self.flex.axis, self.flex.align) {
            (FlexAxis::Row, Align::FirstBaseline) => Some(0),
            (FlexAxis::Row, Align::LastBaseline) => Some(1),
            _ => None,
        }
    }
}

/// The children of `i` in its flow (not its background or overlay).
fn flow(items: &[Item], i: usize) -> Vec<usize> {
    items[i].children.iter().copied().filter(|&c| items[c].role == Role::Flow).collect()
}

/// The children of `i` laid over its frame.
fn layered(items: &[Item], i: usize) -> Vec<usize> {
    items[i].children.iter().copied().filter(|&c| items[c].role != Role::Flow).collect()
}

/// Lay out the tree under `root` into `items[..].out`. `root_room` is the room the root may fill (the viewport).
pub fn solve(items: &mut [Item], root: usize, root_room: [f32; 2]) {
    pass(items, root, root_room);
    if items.iter().any(|it| it.safe_pad.contains(&true)) {
        // Where a node is depends on how much the nodes before it padded: pad from the last pass's places until the pads
        // stop changing (two more passes for a column of padded nodes).
        let base: Vec<[f32; 4]> = items.iter().map(|it| it.pad).collect();
        for _ in 0..4 {
            let before: Vec<[f32; 4]> = items.iter().map(|it| it.pad).collect();
            safe_area(items, root, None, &base);
            if items.iter().zip(&before).all(|(it, b)| it.pad == *b) {
                break;
            }
            pass(items, root, root_room);
        }
    }
}

fn pass(items: &mut [Item], root: usize, room: [f32; 2]) {
    for it in items.iter_mut() {
        it.hidden = false;
        it.range = None;
        it.cols.clear();
    }
    natural(items, root, 0);
    let w = root_size(&items[root], 0, room[0]);
    assign(items, root, 0, w);
    natural(items, root, 1);
    let h = root_size(&items[root], 1, room[1]);
    assign(items, root, 1, h);
    if grow_limits(items) {
        natural(items, root, 1);
        let h = root_size(&items[root], 1, room[1]);
        assign(items, root, 1, h);
    }
    let view = [items[root].out.x, items[root].out.y, items[root].out.x + room[0].max(w), items[root].out.y + room[1].max(h)];
    place(items, root, view);
}

/// Cap each `GrowLimit` node's height (its `size[1].max`) at its share of its nearest ancestor whose height does not come
/// from its content, from the heights just assigned. That ancestor's height does not depend on the node's, so one more
/// height pass settles it. Whether a cap moved.
fn grow_limits(items: &mut [Item]) -> bool {
    if items.iter().all(|it| it.limit.is_none()) {
        return false;
    }
    let mut parent = vec![usize::MAX; items.len()];
    for (i, it) in items.iter().enumerate() {
        for &c in &it.children {
            parent[c] = i;
        }
    }
    let mut moved = false;
    for i in 0..items.len() {
        let Some(l) = items[i].limit else { continue };
        let mut p = parent[i];
        while p != usize::MAX && items[p].size[1].kind == SizeKind::Fit {
            p = parent[p];
        }
        let cap = if p == usize::MAX {
            l.max
        } else {
            let c = (l.share * items[p].out.h - l.keep).max(items[i].size[1].min).max(1.0);
            if l.max > 0.0 { c.min(l.max) } else { c }
        };
        if (items[i].size[1].max - cap).abs() > 0.01 {
            items[i].size[1].max = cap;
            moved = true;
        }
    }
    moved
}

/// Pad every `SafeAreaPadding` node by how far it reaches into the inset of the nearest `SafeAreaInset` above it (`safe`, as
/// the rectangle that is safe: x0, y0, x1, y1), from where the first pass put it. Its children are then inside the safe
/// area, so none of them pads again.
fn safe_area(items: &mut [Item], i: usize, safe: Option<[f32; 4]>, base: &[[f32; 4]]) {
    let o = items[i].out;
    let mut safe = safe;
    let s = items[i].safe;
    if s != [0.0; 4] {
        let mine = [o.x + s[0], o.y + s[2], o.x + o.w - s[1], o.y + o.h - s[3]];
        safe = Some(safe.map_or(mine, |r| [r[0].max(mine[0]), r[1].max(mine[1]), r[2].min(mine[2]), r[3].min(mine[3])]));
    }
    let flags = items[i].safe_pad;
    if let Some(r) = safe.filter(|_| flags.contains(&true)) {
        let extra = [r[0] - o.x, o.x + o.w - r[2], r[1] - o.y, o.y + o.h - r[3]];
        for k in 0..4 {
            items[i].pad[k] = base[i][k] + if flags[k] { extra[k].max(0.0) } else { 0.0 };
        }
        safe = None;
    }
    for c in items[i].children.clone() {
        safe_area(items, c, safe, base);
    }
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

/// How many rows a `Lazy` container's `count` makes in `cols` columns, and how long they run with `gap` between.
fn lazy_extent(l: LazySpec, cols: usize, gap: f32) -> f32 {
    let rows = l.count.div_ceil(cols.max(1) as u32);
    rows as f32 * l.row + gap * rows.saturating_sub(1) as f32
}

/// The rows in view of a `Lazy` container: `lo..hi` is the stretch of its content in view (points from its content's start,
/// along its main axis), rows are `row` long with `gap` between and `cols` to a row (a lazy grid), and `overscan` more rows
/// are kept either side. Returns the item indices `first..last`.
#[must_use]
pub fn lazy_range(lo: f32, hi: f32, row: f32, gap: f32, count: u32, cols: u32, overscan: u32) -> (u32, u32) {
    let cols = cols.max(1);
    let rows = count.div_ceil(cols);
    let stride = (row + gap).max(1.0);
    if hi <= lo || rows == 0 {
        return (0, 0);
    }
    // The first row whose far edge is past `lo`, the last whose near edge is before `hi`.
    let first = ((lo - row) / stride).floor().max(-1.0) as i64 + 1;
    let last = (hi / stride).ceil().max(0.0) as i64;
    let first = (first - i64::from(overscan)).clamp(0, i64::from(rows)) as u32;
    let last = (last + i64::from(overscan)).clamp(0, i64::from(rows)) as u32;
    if last <= first {
        return (0, 0);
    }
    (first * cols, (last * cols).min(count))
}

/// Columns of a `GridColumns` container `inner` wide with `gap` between: how many and how wide.
fn adaptive_cols(inner: f32, gap: f32, min: f32, max: f32) -> (usize, f32) {
    let n = (((inner + gap) / (min.max(1.0) + gap)).floor() as usize).max(1);
    let w = ((inner - gap * (n - 1) as f32) / n as f32).max(0.0);
    (n, if max > 0.0 { w.min(max) } else { w })
}

fn natural(items: &mut [Item], i: usize, a: usize) {
    let kids = items[i].children.clone();
    for &c in &kids {
        natural(items, c, a);
    }
    let flow = flow(items, i);
    let it = &items[i];
    // A `Lazy` container is as long as all its rows whether or not any are made: its rows come from the range this
    // length puts in view, so measuring only the made ones would let a container with none stay empty for good.
    let content = if let Some(l) = it.lazy.filter(|_| it.main(a) || (it.adaptive.is_some() && a == 1)) {
        lazy_extent(l, it.cols.len(), it.flex.gap)
    } else if flow.is_empty() {
        it.intrinsic[a]
    } else if it.fits {
        // As wide as its widest variant would like; as tall as the one chosen (width runs first).
        if a == 0 { items[flow[0]].nat[0] } else { items[flow[it.chosen.min(flow.len() - 1)]].nat[1] }
    } else if let Some(g) = &it.grid {
        grid_natural(items, i, g, a)
    } else if let Some([min, _]) = it.adaptive {
        if a == 0 {
            flow.iter().map(|&c| contribution(&items[c], 0)).fold(min, f32::max)
        } else {
            let n = it.cols.len().max(1);
            let rows: Vec<f32> = flow.chunks(n).map(|r| r.iter().map(|&c| items[c].nat[1]).fold(0.0, f32::max)).collect();
            rows.iter().sum::<f32>() + it.flex.gap * rows.len().saturating_sub(1) as f32
        }
    } else if it.flex.axis == FlexAxis::Z {
        flow.iter().map(|&c| contribution(&items[c], a)).fold(0.0, f32::max)
    } else if it.main(a) {
        let sum: f32 = flow.iter().map(|&c| contribution(&items[c], a)).sum();
        sum + it.flex.gap * (flow.len() - 1) as f32
    } else if it.wrapping() {
        let lines = lines(items, i);
        let heights: f32 = lines.iter().map(|l| l.iter().map(|&c| items[c].nat[1]).fold(0.0, f32::max)).sum();
        heights + it.flex.gap * lines.len().saturating_sub(1) as f32
    } else if let Some(k) = it.baseline_row().filter(|_| a == 1) {
        // Baselines on one line: the most any child reaches above it plus the most any reaches below.
        let (up, down) = flow.iter().fold((0.0f32, 0.0f32), |(u, d), &c| {
            let ch = &items[c];
            let b = ch.bl[k].unwrap_or(ch.nat[1]);
            (u.max(b), d.max(ch.nat[1] - b))
        });
        up + down
    } else {
        flow.iter().map(|&c| contribution(&items[c], a)).fold(0.0, f32::max)
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
    let last_variant = flow.last().map_or(0.0, |&c| items[c].nat[0]);
    let it = &mut items[i];
    it.floor[a] = if it.fixed[a] {
        it.nat[a]
    } else if it.fits && a == 0 {
        it.size[0].min.max(last_variant)
    } else {
        it.size[a].min
    };
    if a == 1 {
        let bl = baselines(items, i, &flow);
        items[i].bl = bl;
    }
}

/// The first and last baselines of `i` from its top at its natural size, from its text or its children's.
fn baselines(items: &[Item], i: usize, flow: &[usize]) -> [Option<f32>; 2] {
    let it = &items[i];
    if flow.is_empty() {
        return [0, 1].map(|k| (it.baseline[k] > 0.0).then(|| it.pad[2] + it.baseline[k]));
    }
    let column = it.flex.axis == FlexAxis::Column && it.grid.is_none() && it.adaptive.is_none() && it.lazy.is_none() && !it.fits;
    if column {
        // Down a column: the first child's first baseline, the last child's last, with what lies before and after them.
        let first = flow.iter().scan(it.pad[2], |top, &c| {
            let at = *top;
            *top += items[c].nat[1] + it.flex.gap;
            Some(items[c].bl[0].map(|b| at + b))
        });
        let last = flow.iter().rev().scan(it.nat[1] - it.pad[3], |bottom, &c| {
            let ch = &items[c];
            *bottom -= ch.nat[1];
            let at = *bottom;
            *bottom -= it.flex.gap;
            Some(ch.bl[1].map(|b| at + b))
        });
        return [first.flatten().next(), last.flatten().next()];
    }
    if let Some(k) = it.baseline_row() {
        let up = flow.iter().map(|&c| items[c].bl[k].unwrap_or(items[c].nat[1])).fold(0.0, f32::max);
        return [Some(it.pad[2] + up), Some(it.pad[2] + up)];
    }
    let visible: Vec<usize> = if it.fits { flow.get(it.chosen).copied().into_iter().collect() } else { flow.to_vec() };
    [
        visible.iter().find_map(|&c| items[c].bl[0]).map(|b| it.pad[2] + b),
        visible.iter().rev().find_map(|&c| items[c].bl[1]).map(|b| it.pad[2] + b),
    ]
}

/// A `Grid`'s rows (its flow children) and each row's cells (a `GridRow`'s flow children; any other child is one cell
/// spanning every column, `None`).
fn grid_rows(items: &[Item], i: usize) -> Vec<(usize, Option<Vec<usize>>)> {
    flow(items, i).into_iter().map(|r| (r, items[r].grid_row.then(|| flow(items, r)))).collect()
}

/// The natural widths of a `Grid`'s columns: a `Fixed` column's own, else its widest cell (within its min and max).
fn grid_cols(items: &[Item], g: &GridSpec, rows: &[(usize, Option<Vec<usize>>)]) -> Vec<f32> {
    let n = rows.iter().filter_map(|(_, c)| c.as_ref().map(Vec::len)).max().unwrap_or(0).max(g.cols.len());
    (0..n)
        .map(|j| {
            let widest = rows.iter().filter_map(|(_, c)| c.as_ref()?.get(j)).map(|&c| contribution(&items[c], 0)).fold(0.0, f32::max);
            match g.cols.get(j) {
                Some((_, s)) if s.kind == SizeKind::Fixed => s.value,
                Some((_, s)) => s.clamp(widest),
                None => widest,
            }
        })
        .collect()
}

fn grid_natural(items: &[Item], i: usize, g: &GridSpec, a: usize) -> f32 {
    let rows = grid_rows(items, i);
    if a == 0 {
        let cols = grid_cols(items, g, &rows);
        let spans = rows.iter().filter(|(_, c)| c.is_none()).map(|(r, _)| contribution(&items[*r], 0)).fold(0.0, f32::max);
        (cols.iter().sum::<f32>() + g.hgap * cols.len().saturating_sub(1) as f32).max(spans)
    } else {
        let hs: f32 = rows.iter().map(|(r, c)| grid_row_height(items, *r, c.as_deref())).sum();
        hs + g.vgap * rows.len().saturating_sub(1) as f32
    }
}

fn grid_row_height(items: &[Item], row: usize, cells: Option<&[usize]>) -> f32 {
    match cells {
        Some(cells) => cells.iter().map(|&c| contribution(&items[c], 1)).fold(0.0, f32::max) + items[row].pads(1),
        None => items[row].nat[1],
    }
}

/// Break a wrapping Row's children into lines by their assigned widths.
fn lines(items: &[Item], i: usize) -> Vec<Vec<usize>> {
    let it = &items[i];
    let inner = (it.out.w - it.pads(0)).max(0.0);
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut used = 0.0;
    for c in flow(items, i) {
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

/// The size a child gets across its parent (or in a slot `inner` long): its own Fixed or Percent, all of it when it
/// grows or is stretched, else its natural size. Across a width (`a` 0) the room is a proposal (SwiftUI's): a Fit child
/// takes the less of its ideal width and the room, down to its floor, unless it is `FixedSize` (see the module doc). A
/// scroll keeps its content's natural length along its main axis only, so this holds across it too. Heights across are
/// left alone (a node is as tall as its content at its width; the parent clips).
fn across_size(ch: &Item, a: usize, inner: f32, align: Align, stretch_fit: bool, grow: bool) -> f32 {
    let s = ch.size[a];
    let align = ch.align_self.unwrap_or(align);
    match s.kind {
        SizeKind::Fixed => s.value,
        SizeKind::Percent => s.clamp(s.value * inner),
        SizeKind::Grow if grow && !ch.fixed[a] => s.clamp(inner),
        SizeKind::Fit if align == Align::Stretch && (stretch_fit || ch.align_self == Some(Align::Stretch)) && !ch.fixed[a] => s.clamp(inner),
        SizeKind::Fit if a == 0 && !ch.fixed[0] => ch.nat[0].min(inner.max(ch.floor[0])),
        _ => ch.nat[a],
    }
}

fn assign(items: &mut [Item], i: usize, a: usize, size: f32) {
    items[i].set_dim(a, size);
    // Layered children take its frame, now and at rest.
    let rest = items[i].rest_room[a];
    for c in layered(items, i) {
        items[c].rest_room[a] = rest;
        assign(items, c, a, size);
    }
    let kids = flow(items, i);
    if kids.is_empty() {
        return;
    }
    // Only a flex container says what its children get at rest (below); inside any other they lay out as they are.
    for &c in &kids {
        items[c].rest_room[a] = None;
    }
    let it = &items[i];
    // A `LaysAtRest` node lays its flow children in the room it will have at rest; its own frame (and its layered
    // children, above) is the room it has now, and `place` puts the children in that frame by its own alignment.
    let inner = (it.rest_room[a].filter(|_| it.at_rest).unwrap_or(size) - it.pads(a)).max(0.0);
    // The room its children get at rest, where that differs from `inner` (inside a `LaysAtRest` node it does not).
    let rest_inner = it.rest_room[a].filter(|_| !it.at_rest).map(|r| (r - it.pads(a)).max(0.0));
    if it.fits {
        if a == 0 {
            let k = kids.iter().position(|&c| items[c].nat[0] <= inner + 0.5).unwrap_or(kids.len() - 1);
            items[i].chosen = k;
        }
        let k = items[i].chosen;
        for (j, &c) in kids.iter().enumerate() {
            if j == k {
                let ch = &items[c];
                let s = match ch.size[a].kind {
                    SizeKind::Fixed => ch.size[a].value,
                    SizeKind::Grow | SizeKind::Percent => ch.size[a].clamp(inner),
                    SizeKind::Fit if a == 0 => ch.nat[0].min(inner),
                    SizeKind::Fit => ch.nat[1],
                };
                assign(items, c, a, s);
            } else {
                items[c].hidden = true;
                assign(items, c, a, 0.0);
            }
        }
        return;
    }
    if let Some(g) = it.grid.clone() {
        grid_assign(items, i, &g, a, inner);
        return;
    }
    if let Some([min, max]) = it.adaptive {
        let (gap, align, lazy) = (it.flex.gap, it.flex.align, it.lazy);
        let cw = if a == 0 {
            let (n, cw) = adaptive_cols(inner, gap, min, max);
            items[i].cols = vec![cw; n];
            cw
        } else {
            0.0
        };
        for c in kids {
            let ch = &items[c];
            let s = match (a, lazy) {
                (0, _) => across_size(ch, 0, cw, align, true, true).min(if ch.size[0].kind == SizeKind::Fixed { f32::MAX } else { cw }),
                (_, Some(l)) => l.row,
                _ => across_size(ch, 1, ch.nat[1], align, false, false),
            };
            assign(items, c, a, s);
        }
        return;
    }
    if let Some(l) = it.lazy.filter(|_| it.main(a)) {
        for c in kids {
            assign(items, c, a, l.row);
        }
        return;
    }
    let main = it.main(a) && !(it.wrapping() && a == 0);
    let (align, z, grow) = (it.flex.align, it.flex.axis == FlexAxis::Z, !(it.wrapping() && a == 1));
    let sizes: Vec<f32> = if main {
        distribute(items, i, &kids, a, inner)
    } else {
        kids.iter().map(|&c| across_size(&items[c], a, inner, align, !z, grow)).collect()
    };
    if let Some(rest) = rest_sizes(items, i, &kids, a, main, rest_inner.unwrap_or(inner), rest_inner.is_some()) {
        for ((&c, &now), r) in kids.iter().zip(&sizes).zip(rest) {
            items[c].rest_room[a] = ((r - now).abs() > 0.01).then_some(r);
        }
    }
    for (&c, s) in kids.iter().zip(sizes) {
        assign(items, c, a, s);
    }
}

/// A `Grid`'s columns on the width pass (Grow columns share what is spare; too little shrinks the rest), then each row and
/// cell; on the height pass each row as tall as its tallest cell.
fn grid_assign(items: &mut [Item], i: usize, g: &GridSpec, a: usize, inner: f32) {
    let rows = grid_rows(items, i);
    if a == 0 {
        let mut cols = grid_cols(items, g, &rows);
        let gaps = g.hgap * cols.len().saturating_sub(1) as f32;
        let free = inner - gaps - cols.iter().sum::<f32>();
        let kind = |j: usize| g.cols.get(j).map_or(SizeKind::Fit, |(_, s)| s.kind);
        if free > 0.0 {
            let grow: Vec<usize> = (0..cols.len()).filter(|&j| kind(j) == SizeKind::Grow).collect();
            let total: f32 = grow.iter().map(|&j| g.cols[j].1.value.max(f32::from(g.cols[j].1.value <= 0.0))).sum();
            for &j in &grow {
                let s = g.cols[j].1;
                cols[j] = s.clamp(cols[j] + free * s.value.max(f32::from(s.value <= 0.0)) / total);
            }
        } else if free < 0.0 {
            let soft: Vec<usize> = (0..cols.len()).filter(|&j| kind(j) != SizeKind::Fixed).collect();
            let total: f32 = soft.iter().map(|&j| cols[j]).sum();
            if total > 0.0 {
                for &j in &soft {
                    let min = g.cols.get(j).map_or(0.0, |(_, s)| s.min);
                    cols[j] = (cols[j] + free * cols[j] / total).max(min);
                }
            }
        }
        items[i].cols.clone_from(&cols);
        for (r, cells) in rows {
            let Some(cells) = cells else {
                let s = across_size(&items[r], 0, inner, Align::Stretch, true, true);
                assign(items, r, 0, s);
                continue;
            };
            items[r].set_dim(0, inner);
            for c in layered(items, r) {
                assign(items, c, 0, inner);
            }
            for (j, &c) in cells.iter().enumerate() {
                let col = cols.get(j).copied().unwrap_or(0.0);
                let align = g.cols.get(j).map_or(g.align, |(al, _)| *al);
                let ch = &items[c];
                let s = across_size(ch, 0, col, align, true, true);
                let s = if ch.size[0].kind == SizeKind::Fixed { s } else { s.min(col) };
                assign(items, c, 0, s);
            }
        }
    } else {
        for (r, cells) in rows {
            let h = grid_row_height(items, r, cells.as_deref());
            let Some(cells) = cells else {
                assign(items, r, 1, h);
                continue;
            };
            items[r].set_dim(1, h);
            for c in layered(items, r) {
                assign(items, c, 1, h);
            }
            let inner_h = h - items[r].pads(1);
            for c in cells {
                let s = across_size(&items[c], 1, inner_h, g.valign, true, true);
                assign(items, c, 1, s);
            }
        }
    }
}

/// The sizes `kids` (the flow children of flex container `i`) get on axis `a` once every moving `RestSize` rests: in
/// `inner`, the container's room at rest (`moved`: it differs from its room now), with each of `kids` whose `RestSize`
/// is along this main axis held at it ([`distribute`] with those as fixed lengths). None when nothing moves here or above,
/// so the sizes they have now are their sizes at rest.
fn rest_sizes(items: &mut [Item], i: usize, kids: &[usize], a: usize, main: bool, inner: f32, moved: bool) -> Option<Vec<f32>> {
    let moving: Vec<(usize, f32)> = kids
        .iter()
        .filter(|_| main)
        .filter_map(|&c| items[c].rest.map(|r| (c, r)))
        .filter(|&(c, r)| items[c].size[a].kind != SizeKind::Fixed || (items[c].size[a].value - r).abs() > 0.01)
        .collect();
    if moving.is_empty() && !moved {
        return None;
    }
    if !main {
        let it = &items[i];
        let (align, z, grow) = (it.flex.align, it.flex.axis == FlexAxis::Z, !(it.wrapping() && a == 1));
        return Some(kids.iter().map(|&c| across_size(&items[c], a, inner, align, !z, grow)).collect());
    }
    let held: Vec<(usize, Sizing)> = moving.iter().map(|&(c, _)| (c, items[c].size[a])).collect();
    for &(c, r) in &moving {
        items[c].size[a] = Sizing::fixed(r);
    }
    let sizes = distribute(items, i, kids, a, inner);
    for (c, s) in held {
        items[c].size[a] = s;
    }
    Some(sizes)
}

/// Main-axis sizes of `kids` (the flow children of `i`): bases, then free room to Grow by weight, or a deficit shrunk off
/// the rest, the lowest `LayoutPriority` first, each down to its floor before the next gives any.
fn distribute(items: &[Item], i: usize, kids: &[usize], a: usize, inner: f32) -> Vec<f32> {
    let it = &items[i];
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
                SizeKind::Grow if ch.fixed[a] => ch.nat[a],
                SizeKind::Grow => s.min,
            }
        })
        .collect();
    let mut free = inner - gaps - sizes.iter().sum::<f32>();
    if free > 0.0 {
        let mut open: Vec<usize> = (0..kids.len()).filter(|&k| items[kids[k]].size[a].kind == SizeKind::Grow && !items[kids[k]].fixed[a]).collect();
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
    } else if free < -0.01 && !it.scrolls {
        let soft = |k: usize| matches!(items[kids[k]].size[a].kind, SizeKind::Fit | SizeKind::Grow) && !items[kids[k]].fixed[a];
        let mut levels: Vec<f32> = (0..kids.len()).filter(|&k| soft(k)).map(|k| items[kids[k]].priority).collect();
        levels.sort_by(f32::total_cmp);
        levels.dedup();
        for level in levels {
            let mut shrinkable: Vec<usize> = (0..kids.len()).filter(|&k| soft(k) && items[kids[k]].priority == level).collect();
            while free < -0.01 && !shrinkable.is_empty() {
                let total: f32 = shrinkable.iter().map(|&k| sizes[k]).sum();
                if total <= 0.0 {
                    break;
                }
                let mut floored = Vec::new();
                let deficit = -free;
                for &k in &shrinkable {
                    let min = items[kids[k]].floor[a];
                    let want = sizes[k] - deficit * sizes[k] / total;
                    if want <= min {
                        floored.push(k);
                    }
                    let got = want.max(min).min(sizes[k]);
                    free += sizes[k] - got;
                    sizes[k] = got;
                }
                shrinkable.retain(|k| !floored.contains(k));
                if floored.is_empty() {
                    break;
                }
            }
            if free >= -0.01 {
                break;
            }
        }
    }
    sizes
}

/// Where a child `size` long sits in `room` by `align` (a baseline alignment, placed afterwards, starts at the start).
fn across(align: Align, room: f32, size: f32) -> f32 {
    match align {
        Align::Start | Align::Stretch | Align::FirstBaseline | Align::LastBaseline => 0.0,
        Align::Center => (room - size) / 2.0,
        Align::End => room - size,
    }
}

/// The first (`k` 0) or last (`k` 1) baseline of the placed subtree under `i`, in window points.
fn placed_baseline(items: &[Item], i: usize, k: usize) -> Option<f32> {
    let it = &items[i];
    let kids: Vec<usize> = flow(items, i).into_iter().filter(|&c| !items[c].hidden).collect();
    if kids.is_empty() {
        return (it.baseline[k] > 0.0).then(|| it.out.y + it.pad[2] + it.baseline[k]);
    }
    if k == 0 { kids.iter().find_map(|&c| placed_baseline(items, c, 0)) } else { kids.iter().rev().find_map(|&c| placed_baseline(items, c, 1)) }
}

fn place(items: &mut [Item], i: usize, view: [f32; 4]) {
    let (flex, pad, scroll, centre, x0, y0, w, h) = {
        let it = &items[i];
        (it.flex, it.pad, it.scroll, it.centre, it.out.x, it.out.y, it.out.w, it.out.h)
    };
    let view = if items[i].clip { [view[0].max(x0), view[1].max(y0), view[2].min(x0 + w), view[3].min(y0 + h)] } else { view };
    for c in layered(items, i) {
        items[c].out.x = x0;
        items[c].out.y = y0;
        place(items, c, view);
    }
    let kids = flow(items, i);
    let (ox, oy) = (x0 + pad[0] - scroll[0], y0 + pad[2] - scroll[1]);
    let (iw, ih) = ((w - pad[0] - pad[1]).max(0.0), (h - pad[2] - pad[3]).max(0.0));
    if kids.is_empty() {
        // A `Lazy` container with no rows made yet still says which are in view, or none would ever be made.
        if items[i].lazy.is_some() {
            lazy_place(items, i, &kids, [ox, oy, iw, ih], view);
        }
        return;
    }
    let mut done = vec![false; kids.len()];
    if items[i].fits {
        for &c in &kids {
            let (cw, ch) = (items[c].out.w, items[c].out.h);
            items[c].out.x = ox + across(flex.align, iw, cw);
            items[c].out.y = oy + across(flex.align, ih, ch);
        }
    } else if let Some(g) = items[i].grid.clone() {
        grid_place(items, i, &g, [ox, oy], view);
        return;
    } else if items[i].adaptive.is_some() || items[i].lazy.is_some() {
        lazy_place(items, i, &kids, [ox, oy, iw, ih], view);
    } else {
        match flex.axis {
            FlexAxis::Z => {
                // Across by `align`, down by `justify` (SwiftUI's two-way `ZStack(alignment:)`); a child's AlignSelf places
                // it both ways.
                let down = match flex.justify {
                    Justify::Center => Align::Center,
                    Justify::End => Align::End,
                    Justify::Start | Justify::SpaceBetween => Align::Start,
                };
                for &c in &kids {
                    let (cw, ch, g) = (items[c].out.w, items[c].out.h, items[c].guide);
                    let (ax, ay) = items[c].align_self.map_or((flex.align, down), |a| (a, a));
                    items[c].out.x = ox + across(ax, iw, cw) - g[0];
                    items[c].out.y = oy + across(ay, ih, ch) - g[1];
                }
            }
            FlexAxis::Row if flex.wrap => {
                let mut y = oy;
                let ls = lines(items, i);
                for line in ls {
                    let lh = line.iter().map(|&c| items[c].out.h).fold(0.0, f32::max);
                    let mut x = ox;
                    for c in line {
                        let al = items[c].align_self.unwrap_or(flex.align);
                        items[c].out.x = x;
                        items[c].out.y = y + across(al, lh, items[c].out.h) - items[c].guide[1];
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
                    let al = items[c].align_self.unwrap_or(flex.align);
                    let cross = across(al, cross_room, items[c].dim(1 - a)) - items[c].guide[1 - a];
                    if a == 0 {
                        items[c].out.x = ox + at;
                        items[c].out.y = oy + cross;
                    } else {
                        items[c].out.x = ox + cross;
                        items[c].out.y = oy + at;
                    }
                    at += s + step;
                }
                if a == 0 {
                    baseline_align(items, &kids, flex.align, oy, view, &mut done);
                }
            }
        }
    }
    for (c, done) in kids.into_iter().zip(done) {
        if !done {
            place(items, c, view);
        }
    }
}

/// In a Row, line up the children aligned on a baseline: place each (so its text is where it will be), then move it down
/// so its baseline meets the lowest-reaching one. A child without a baseline lines up by its bottom.
fn baseline_align(items: &mut [Item], kids: &[usize], align: Align, oy: f32, view: [f32; 4], done: &mut [bool]) {
    let which = |al: Align| match al {
        Align::FirstBaseline => Some(0),
        Align::LastBaseline => Some(1),
        _ => None,
    };
    let mut on: Vec<(usize, usize, f32)> = Vec::new();
    for (n, &c) in kids.iter().enumerate() {
        let Some(k) = which(items[c].align_self.unwrap_or(align)) else { continue };
        place(items, c, view);
        done[n] = true;
        let top = items[c].out.y;
        let b = placed_baseline(items, c, k).map_or(items[c].out.h, |b| b - top);
        on.push((c, k, b));
    }
    let line = on.iter().map(|&(_, _, b)| b).fold(0.0, f32::max);
    for (c, _, b) in on {
        let dy = oy + line - b - items[c].out.y;
        if dy != 0.0 {
            shift(items, c, 0.0, dy);
        }
    }
}

/// A `Grid`'s rows down its column and cells across, each cell aligned in its column (`ox`, `oy` its content's corner).
fn grid_place(items: &mut [Item], i: usize, g: &GridSpec, [ox, oy]: [f32; 2], view: [f32; 4]) {
    let cols = items[i].cols.clone();
    let mut y = oy;
    for (r, cells) in grid_rows(items, i) {
        items[r].out.x = ox;
        items[r].out.y = y;
        let rh = items[r].out.h;
        if let Some(cells) = cells {
            let (rx, ry) = (ox + items[r].pad[0], y + items[r].pad[2]);
            let inner_h = rh - items[r].pads(1);
            let mut x = rx;
            for (j, &c) in cells.iter().enumerate() {
                let col = cols.get(j).copied().unwrap_or(0.0);
                let al = items[c].align_self.unwrap_or(g.cols.get(j).map_or(g.align, |(al, _)| *al));
                let (cw, ch, gd) = (items[c].out.w, items[c].out.h, items[c].guide);
                items[c].out.x = x + across(al, col, cw) - gd[0];
                items[c].out.y = ry + across(g.valign, inner_h, ch) - gd[1];
                x += col + g.hgap;
            }
            for c in layered(items, r) {
                items[c].out.x = ox;
                items[c].out.y = y;
            }
        }
        y += rh + g.vgap;
    }
    // Rows that are GridRows hold cells placed above; place their subtrees (a spanning row lays itself out).
    for (r, cells) in grid_rows(items, i) {
        match cells {
            Some(cells) => {
                for c in layered(items, r).into_iter().chain(cells) {
                    place(items, c, view);
                }
            }
            None => place(items, r, view),
        }
    }
}

/// The children of a `GridColumns` or `Lazy` container: row by row in its columns (one column for a plain lazy stack), a
/// lazy child at the row its `LazyIndex` says. A lazy container also says which rows are in `view`.
fn lazy_place(items: &mut [Item], i: usize, kids: &[usize], [ox, oy, iw, ih]: [f32; 4], view: [f32; 4]) {
    let (flex, lazy) = (items[i].flex, items[i].lazy);
    let a = if items[i].adaptive.is_some() { 1 } else { items[i].main_axis() };
    let n = items[i].cols.len().max(1);
    let cw = items[i].cols.first().copied().unwrap_or(if a == 1 { iw } else { ih });
    if let Some(l) = lazy {
        let start = if a == 0 { ox } else { oy };
        let (lo, hi) = if a == 0 { (view[0] - start, view[2] - start) } else { (view[1] - start, view[3] - start) };
        items[i].range = Some(lazy_range(lo, hi, l.row, flex.gap, l.count, n as u32, l.overscan));
        for (k, &c) in kids.iter().enumerate() {
            let idx = items[c].lazy_index.unwrap_or(k as u32) as usize;
            let (r, col) = (idx / n, idx % n);
            let main = r as f32 * (l.row + flex.gap);
            let cross = if items[i].adaptive.is_some() { col as f32 * (cw + flex.gap) } else { 0.0 };
            let al = items[c].align_self.unwrap_or(flex.align);
            let room = if items[i].adaptive.is_some() { cw } else if a == 1 { iw } else { ih };
            let off = cross + across(al, room, items[c].dim(1 - a)) - items[c].guide[1 - a];
            if a == 1 {
                items[c].out.x = ox + off;
                items[c].out.y = oy + main;
            } else {
                items[c].out.x = ox + main;
                items[c].out.y = oy + off;
            }
        }
        return;
    }
    let mut y = oy;
    for row in kids.chunks(n) {
        let rh = row.iter().map(|&c| items[c].out.h).fold(0.0, f32::max);
        for (col, &c) in row.iter().enumerate() {
            let al = items[c].align_self.unwrap_or(flex.align);
            items[c].out.x = ox + col as f32 * (cw + flex.gap) + across(al, cw, items[c].out.w) - items[c].guide[0];
            items[c].out.y = y - items[c].guide[1];
        }
        y += rh + flex.gap;
    }
}

/// Where an anchored overlay of `size` goes against `anchor` on `side` (its edge, nudge and alignment along the edge),
/// flipped to the opposite side when it overflows and then slid to stay inside `room` (`x, y, w, h`: the viewport from its
/// origin, or for a windowed overlay the screen's visible frame, which starts wherever the screen is), as NSPopover and
/// NSMenu flip and shift against the screen.
#[must_use]
pub fn anchored(anchor: Laid, size: [f32; 2], side: Side, room: [f32; 4]) -> [f32; 2] {
    let (edge, nudge) = (side.edge, [side.dx, side.dy]);
    let [rx, ry, rw, rh] = room;
    // Along the edge: from the anchor's start, by how much the overlay's start sits past it.
    let along = |room: f32, len: f32| match side.align {
        Align::Center => (room - len) / 2.0,
        Align::End => room - len,
        _ => 0.0,
    };
    let (ax, ay) = (anchor.x + along(anchor.w, size[0]), anchor.y + along(anchor.h, size[1]));
    let at = |e: Opens| match e {
        Opens::Bottom => [ax + nudge[0], anchor.y + anchor.h + nudge[1]],
        Opens::Top => [ax + nudge[0], anchor.y - size[1] - nudge[1]],
        Opens::Right => [anchor.x + anchor.w + nudge[0], ay + nudge[1]],
        Opens::Left => [anchor.x - size[0] - nudge[0], ay + nudge[1]],
    };
    let fits = |p: [f32; 2]| p[0] >= rx && p[1] >= ry && p[0] + size[0] <= rx + rw && p[1] + size[1] <= ry + rh;
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
    [p[0].min(rx + rw - size[0]).max(rx), p[1].min(ry + rh - size[1]).max(ry)]
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

    fn row(gap: f32, align: Align, children: Vec<usize>) -> Item {
        Item { flex: Flex { axis: FlexAxis::Row, gap, align, ..Flex::default() }, ..item([Sizing::GROW, Sizing::FIT], children) }
    }

    /// A text leaf `w` wide and `h` tall whose first and last baselines are `b0` and `b1` from its top.
    fn text(w: f32, h: f32, b0: f32, b1: f32) -> Item {
        Item { intrinsic: [w, h], baseline: [b0, b1], ..Item::default() }
    }

    /// A windowed overlay is kept in the screen's room, which may start left of and above the viewport: a menu with no
    /// room above it in the window opens above it on the screen, past the window's edge, and one at the screen's edge
    /// slides back onto it.
    #[test]
    fn an_overlay_is_kept_in_a_room_that_starts_anywhere() {
        let side = Side { edge: Opens::Top, dx: 0.0, dy: 6.0, align: Align::Start };
        let anchor = Laid { x: 10.0, y: 40.0, w: 80.0, h: 20.0 };
        // The window's own room: no room above, so it flips below the anchor.
        assert_eq!(anchored(anchor, [200.0, 100.0], side, [0.0, 0.0, 800.0, 600.0]), [10.0, 66.0]);
        // The screen's, 300 points above the window and 50 left of it: it opens above, out of the window.
        assert_eq!(anchored(anchor, [200.0, 100.0], side, [-50.0, -300.0, 1600.0, 1000.0]), [10.0, -66.0]);
        // Slid back in at the screen's left edge.
        let left = Laid { x: -45.0, ..anchor };
        assert_eq!(anchored(left, [200.0, 100.0], side, [-40.0, -300.0, 1600.0, 1000.0]), [-40.0, -66.0]);
    }

    /// A sidebar folding away (its spring leaving it 100 wide, resting at 0) beside a `LaysAtRest` card: the card's frame takes
    /// what the spring leaves it, its content the room it will have once the sidebar rests, so its words are not laid out at
    /// an in-between width. At rest the two are the same.
    #[test]
    fn a_card_lays_its_content_out_at_the_width_it_rests_at() {
        let side = |w: f32| Item { rest: Some(0.0), ..item([Sizing::fixed(w), Sizing::fixed(10.0)], vec![]) };
        let card = Item { at_rest: true, ..item([Sizing::GROW, Sizing::fixed(10.0)], vec![3]) };
        let content = item([Sizing::GROW, Sizing::fixed(10.0)], vec![]);
        let mut items = vec![row(0.0, Align::Start, vec![1, 2]), side(100.0), card, content];
        solve(&mut items, 0, [1000.0, 500.0]);
        assert_eq!((items[2].out.x, items[2].out.w), (100.0, 900.0));
        assert_eq!((items[3].out.x, items[3].out.w), (100.0, 1000.0));
        items[1] = side(0.0);
        solve(&mut items, 0, [1000.0, 500.0]);
        assert_eq!((items[2].out.w, items[3].out.w), (1000.0, 1000.0));
    }

    /// A reading column (at most `max` wide) centred in a `LaysAtRest` list, in a pane beside a sidebar folding open on a
    /// spring (`now` wide, laid against 300: the narrower the card gets): the column takes its width once (the room the list
    /// has once the sidebar is open) and stays centred in the frame the list has this frame, so it glides with the spring,
    /// never laid against one width and centred against another.
    #[test]
    fn a_column_lays_out_at_rest_and_centres_in_the_frame_it_has_now() {
        let lay = |now: f32, max: f32| {
            let side = Item { rest: Some(300.0), ..item([Sizing::fixed(now), Sizing::fixed(10.0)], vec![]) };
            let pane = item([Sizing::GROW, Sizing::fixed(10.0)], vec![3]);
            let list = Item {
                at_rest: true,
                flex: Flex { axis: FlexAxis::Column, align: Align::Center, ..Flex::default() },
                ..item([Sizing::GROW, Sizing::fixed(10.0)], vec![4])
            };
            let column = item([Sizing { max, ..Sizing::GROW }, Sizing::fixed(10.0)], vec![]);
            let mut items = vec![row(0.0, Align::Start, vec![1, 2]), side, pane, list, column];
            solve(&mut items, 0, [1000.0, 500.0]);
            (items[3].out.x, items[3].out.w, items[4].out.x, items[4].out.w)
        };
        // It fits at either width: it never rewraps, only recentres with the frame.
        assert_eq!(lay(0.0, 600.0), (0.0, 1000.0, 200.0, 600.0));
        assert_eq!(lay(150.0, 600.0), (150.0, 850.0, 275.0, 600.0));
        assert_eq!(lay(300.0, 600.0), (300.0, 700.0, 350.0, 600.0));
        // Too wide for the open card: it takes the open card's width from the first frame, centred in the frame as it moves.
        assert_eq!(lay(0.0, 800.0), (0.0, 1000.0, 150.0, 700.0));
        assert_eq!(lay(150.0, 800.0), (150.0, 850.0, 225.0, 700.0));
    }

    /// A card (at most 800 wide, centred) in a pane beside a sidebar folding open (`now` wide, laid against 300), holding words
    /// and a trailing control after a spacer: the card, its words and its control follow the moving frame on every frame,
    /// so the words re-wrap live at the width they have (no `LaysAtRest` here: only a terminal's grid lays out at rest).
    #[test]
    fn words_take_the_width_their_box_has_on_every_frame_of_a_fold() {
        let lay = |now: f32| {
            let side = Item { rest: Some(300.0), ..item([Sizing::fixed(now), Sizing::fixed(10.0)], vec![]) };
            let pane = Item {
                flex: Flex { axis: FlexAxis::Column, align: Align::Center, ..Flex::default() },
                ..item([Sizing::GROW, Sizing::fixed(40.0)], vec![3])
            };
            let card = Item {
                flex: Flex { axis: FlexAxis::Column, align: Align::Stretch, ..Flex::default() },
                ..item([Sizing { max: 800.0, ..Sizing::GROW }, Sizing::fixed(20.0)], vec![4, 5])
            };
            let words = item([Sizing::GROW, Sizing::fixed(10.0)], vec![]);
            let bar = row(0.0, Align::Start, vec![6, 7]);
            let spacer = item([Sizing::GROW, Sizing::fixed(10.0)], vec![]);
            let control = item([Sizing::fixed(20.0), Sizing::fixed(10.0)], vec![]);
            let mut items = vec![row(0.0, Align::Start, vec![1, 2]), side, pane, card, words, bar, spacer, control];
            solve(&mut items, 0, [1000.0, 500.0]);
            let c = items[7].out;
            (items[3].out.x, items[3].out.w, items[4].out.w, c.x + c.w)
        };
        assert_eq!(lay(0.0), (100.0, 800.0, 800.0, 900.0));
        assert_eq!(lay(150.0), (175.0, 800.0, 800.0, 975.0));
        assert_eq!(lay(250.0), (250.0, 750.0, 750.0, 1000.0));
        assert_eq!(lay(300.0), (300.0, 700.0, 700.0, 1000.0));
    }

    /// A row of a fixed box, a spacer and a fixed box, 8 apart in 200 points: the spacer takes what is left.
    #[test]
    fn a_grow_child_takes_the_rest_of_the_row() {
        let mut items = vec![
            row(8.0, Align::Start, vec![1, 2, 3]),
            item([Sizing::fixed(40.0), Sizing::fixed(10.0)], vec![]),
            item([Sizing::GROW, Sizing::FIT], vec![]),
            item([Sizing::fixed(40.0), Sizing::fixed(10.0)], vec![]),
        ];
        solve(&mut items, 0, [200.0, 100.0]);
        assert_eq!(items[2].out.w, 200.0 - 80.0 - 16.0);
        assert_eq!(items[3].out.x, 160.0);
        assert_eq!(items[0].out.h, 10.0);
    }

    /// A 28-point title (baseline 26 of 34) beside 13-point words (baseline 12 of 16) and a box with none: on
    /// `FirstBaseline` both baselines meet 26 down, the box's bottom meets them too, and the row is as tall as the most
    /// above that line plus the most below it.
    #[test]
    fn first_baselines_line_up_across_text_sizes() {
        let mut items = vec![
            row(8.0, Align::FirstBaseline, vec![1, 2, 3]),
            text(100.0, 34.0, 26.0, 26.0),
            text(60.0, 16.0, 12.0, 12.0),
            item([Sizing::fixed(10.0), Sizing::fixed(10.0)], vec![]),
        ];
        solve(&mut items, 0, [400.0, 100.0]);
        let base = |i: usize| items[i].out.y + items[i].baseline[0];
        assert_eq!(base(1), 26.0);
        assert_eq!(base(2), 26.0);
        assert_eq!(items[3].out.y + items[3].out.h, 26.0);
        assert_eq!(items[0].out.h, 34.0);
    }

    /// Two lines of small words (last baseline 30) beside one big line (baseline 26): on `LastBaseline` the last lines
    /// meet, and a baseline inside a padded column counts its padding and the rows above it.
    #[test]
    fn last_baselines_line_up_and_nested_columns_count_their_padding() {
        let col = Item { flex: Flex { axis: FlexAxis::Column, gap: 4.0, ..Flex::default() }, pad: [0.0, 0.0, 6.0, 0.0], ..item([Sizing::FIT, Sizing::FIT], vec![3, 4]) };
        let mut items = vec![
            row(8.0, Align::LastBaseline, vec![1, 2]),
            text(100.0, 34.0, 26.0, 26.0),
            col,
            text(50.0, 16.0, 12.0, 12.0),
            text(50.0, 16.0, 12.0, 12.0),
        ];
        solve(&mut items, 0, [400.0, 200.0]);
        let big = items[1].out.y + 26.0;
        let small_last = items[4].out.y + 12.0;
        assert_eq!(big, small_last);
        // The column's last line sits 6 + 16 + 4 down it, its baseline 12 more: 38 below its top, so the big line drops.
        assert_eq!(items[1].out.y, 38.0 - 26.0);
        assert_eq!(items[2].out.y, 0.0);
    }

    /// A name (priority 0) and a badge (priority 1) that do not both fit in 120 points: the name gives up all it must
    /// (down to its floor) and the badge keeps its natural width. With equal priority both shrink in proportion.
    #[test]
    fn the_lower_priority_child_truncates_first() {
        let name = text(150.0, 16.0, 12.0, 12.0);
        let badge = Item { priority: 1.0, ..text(40.0, 16.0, 12.0, 12.0) };
        let mut items = vec![row(8.0, Align::Center, vec![1, 2]), name, badge];
        solve(&mut items, 0, [120.0, 50.0]);
        assert_eq!(items[2].out.w, 40.0);
        assert_eq!(items[1].out.w, 120.0 - 8.0 - 40.0);
        assert_eq!(items[2].out.x, 120.0 - 40.0);
        // Even priority: proportional, the badge loses some too.
        items[2].priority = 0.0;
        solve(&mut items, 0, [120.0, 50.0]);
        assert!(items[2].out.w < 40.0 && items[1].out.w > 72.0 - 1.0);
        // FixedSize keeps the badge whole whatever its priority.
        items[2].fixed = [true, false];
        solve(&mut items, 0, [120.0, 50.0]);
        assert_eq!(items[2].out.w, 40.0);
        // A floor stops the name: past it, the badge gives the rest.
        items[2].fixed = [false, false];
        items[2].priority = 1.0;
        items[1].size[0].min = 90.0;
        solve(&mut items, 0, [120.0, 50.0]);
        assert_eq!(items[1].out.w, 90.0);
        assert_eq!(items[2].out.w, 120.0 - 8.0 - 90.0);
    }

    /// A chat bubble: a trailing-justified row 300 wide holding a Fit column (padding 14 a side, at most 240) around words
    /// whose one line runs 500. The column takes the less of the room and its max, and its words get its inner width to
    /// wrap at, whatever the column's align; short words keep their own width and the bubble hugs them.
    #[test]
    fn a_fit_box_around_words_wraps_at_the_room_it_is_proposed() {
        let bubble = |words: f32| {
            let col = Item {
                flex: Flex { axis: FlexAxis::Column, align: Align::Start, ..Flex::default() },
                pad: [14.0, 14.0, 8.0, 8.0],
                ..item([Sizing { max: 240.0, ..Sizing::FIT }, Sizing::FIT], vec![2])
            };
            let row = Item { flex: Flex { axis: FlexAxis::Row, justify: Justify::End, ..Flex::default() }, ..item([Sizing::fixed(300.0), Sizing::FIT], vec![1]) };
            vec![row, col, text(words, 20.0, 15.0, 15.0)]
        };
        let mut items = bubble(500.0);
        solve(&mut items, 0, [300.0, 100.0]);
        assert_eq!(items[1].out.w, 240.0);
        assert_eq!(items[1].out.x, 60.0);
        assert_eq!(items[2].out.w, 240.0 - 28.0);
        // No max: the room is the cap.
        items[1].size[0].max = 0.0;
        solve(&mut items, 0, [300.0, 100.0]);
        assert_eq!(items[1].out.w, 300.0);
        assert_eq!(items[2].out.w, 300.0 - 28.0);
        // FixedSize opts out: the words keep their one line.
        items[2].fixed = [true, false];
        solve(&mut items, 0, [300.0, 100.0]);
        assert_eq!(items[2].out.w, 500.0);
        let mut items = bubble(80.0);
        solve(&mut items, 0, [300.0, 100.0]);
        assert_eq!(items[1].out.w, 108.0);
        assert_eq!(items[1].out.x, 300.0 - 108.0);
        assert_eq!(items[2].out.w, 80.0);
    }

    /// Rows 20 long with 4 between (a stride of 24): the view 100..200 of the content shows rows 4 (ending 116) to 8
    /// (starting 192); overscan 2 widens that; a 3-column grid turns rows into item indices; the ends clamp.
    #[test]
    fn lazy_range_covers_the_rows_in_view() {
        assert_eq!(lazy_range(100.0, 200.0, 20.0, 4.0, 1000, 1, 0), (4, 9));
        assert_eq!(lazy_range(100.0, 200.0, 20.0, 4.0, 1000, 1, 2), (2, 11));
        // Exactly on a boundary: row 4 starts at 96 and ends at 116; a view starting at 116 no longer shows it.
        assert_eq!(lazy_range(116.0, 140.0, 20.0, 4.0, 1000, 1, 0), (5, 6));
        assert_eq!(lazy_range(100.0, 200.0, 20.0, 4.0, 1000, 3, 0), (12, 27));
        assert_eq!(lazy_range(-50.0, 30.0, 20.0, 4.0, 1000, 1, 3), (0, 5));
        assert_eq!(lazy_range(23_000.0, 30_000.0, 20.0, 4.0, 1000, 1, 1), (957, 1000));
        assert_eq!(lazy_range(0.0, 100.0, 20.0, 4.0, 0, 1, 1), (0, 0));
        assert_eq!(lazy_range(100.0, 50.0, 20.0, 4.0, 10, 1, 1), (0, 0));
    }

    /// A lazy column of 10 000 rows inside a 100-point scroll clip, scrolled 2 400 points: the container is as tall as all
    /// of them, says rows 100..105 (with overscan 0) are in view, and puts the row with LazyIndex 101 at its place.
    #[test]
    fn a_lazy_column_inside_a_scroll_reports_its_range() {
        let scroll = Item { clip: true, scrolls: true, scroll: [0.0, 2400.0], ..item([Sizing::fixed(200.0), Sizing::fixed(100.0)], vec![1]) };
        let list = Item { lazy: Some(LazySpec { count: 10_000, row: 20.0, overscan: 0 }), flex: Flex { axis: FlexAxis::Column, gap: 4.0, align: Align::Stretch, ..Flex::default() }, ..item([Sizing::GROW, Sizing::FIT], vec![2]) };
        let r = Item { lazy_index: Some(101), ..text(50.0, 16.0, 12.0, 12.0) };
        let mut items = vec![scroll, list, r];
        solve(&mut items, 0, [800.0, 600.0]);
        assert_eq!(items[1].out.h, 10_000.0 * 20.0 + 9_999.0 * 4.0);
        assert_eq!(items[1].range, Some((100, 105)));
        assert_eq!(items[2].out.y, 101.0 * 24.0 - 2400.0);
        assert_eq!(items[2].out.w, 200.0);
        assert_eq!(items[2].out.h, 20.0);
    }

    /// A lazy column with none of its 6 rows made yet (all were out of view on the last frame) is still as tall as the 6
    /// and says all of them are in view now, so the next frame makes them; before, it measured 0 tall, said none were in
    /// view, and stayed an empty bar for good (a wide table under a chat tool card).
    #[test]
    fn a_lazy_column_with_no_rows_made_still_asks_for_them() {
        let scroll = Item { clip: true, scrolls: true, ..item([Sizing::fixed(200.0), Sizing::fixed(400.0)], vec![1]) };
        let list = Item { lazy: Some(LazySpec { count: 6, row: 24.0, overscan: 8 }), flex: Flex { axis: FlexAxis::Column, align: Align::Stretch, ..Flex::default() }, ..item([Sizing::GROW, Sizing::FIT], vec![]) };
        let mut items = vec![scroll, list];
        solve(&mut items, 0, [800.0, 600.0]);
        assert_eq!(items[1].out.h, 6.0 * 24.0);
        assert_eq!(items[1].range, Some((0, 6)));
    }

    /// A 2x2 grid: each column as wide as its widest cell, the second column end-aligned, a spanning row as wide as the grid.
    #[test]
    fn grid_columns_take_their_widest_cell() {
        let g = GridSpec { hgap: 10.0, vgap: 5.0, align: Align::Start, valign: Align::Start, cols: vec![(Align::Start, Sizing::FIT), (Align::End, Sizing::FIT)] };
        let grid = Item { grid: Some(g), ..item([Sizing::FIT, Sizing::FIT], vec![1, 4, 5]) };
        let r1 = Item { grid_row: true, ..item([Sizing::FIT, Sizing::FIT], vec![2, 3]) };
        let r2 = Item { grid_row: true, ..item([Sizing::FIT, Sizing::FIT], vec![6, 7]) };
        let span = item([Sizing::GROW, Sizing::fixed(1.0)], vec![]);
        let mut items = vec![grid, r1, text(30.0, 16.0, 12.0, 12.0), text(80.0, 16.0, 12.0, 12.0), span, r2, text(60.0, 16.0, 12.0, 12.0), text(20.0, 16.0, 12.0, 12.0)];
        solve(&mut items, 0, [800.0, 600.0]);
        assert_eq!(items[0].out.w, 60.0 + 10.0 + 80.0);
        assert_eq!(items[3].out.x, 70.0);
        assert_eq!(items[7].out.x, 70.0 + 80.0 - 20.0);
        assert_eq!(items[4].out.w, 150.0);
        assert_eq!(items[5].out.y, 16.0 + 5.0 + 1.0 + 5.0);
    }

    /// ViewThatFits: a 300-wide variant and a 100-wide one; given 200 it shows the second and hides the first.
    #[test]
    fn view_that_fits_picks_the_first_variant_that_fits() {
        let fits = Item { fits: true, ..item([Sizing::fixed(200.0), Sizing::FIT], vec![1, 2]) };
        let mut items = vec![fits, text(300.0, 16.0, 12.0, 12.0), text(100.0, 30.0, 12.0, 12.0)];
        solve(&mut items, 0, [800.0, 600.0]);
        assert!(items[1].hidden && !items[2].hidden);
        assert_eq!(items[0].out.h, 30.0);
        items[0].size[0] = Sizing::fixed(400.0);
        solve(&mut items, 0, [800.0, 600.0]);
        assert!(!items[1].hidden && items[2].hidden);
    }

    /// A pane 400 tall holding a transcript (Grow) over a foot (Fit) around a growing area of 1 000 points of words,
    /// limited to 0.85 of the pane less 40: the area stops at 300 and the transcript keeps the rest; short words keep
    /// their own height; the area's own `Size.h.max` still caps it.
    #[test]
    fn a_grow_limit_caps_a_fit_height_at_a_share_of_its_pane() {
        let col = |children| Item { flex: Flex { axis: FlexAxis::Column, align: Align::Stretch, ..Flex::default() }, ..item([Sizing::GROW, Sizing::FIT], children) };
        let pane = Item { size: [Sizing::GROW, Sizing::GROW], ..col(vec![1, 2]) };
        let list = Item { size: [Sizing::GROW, Sizing::GROW], ..col(vec![]) };
        let foot = Item { pad: [0.0, 0.0, 10.0, 10.0], ..col(vec![3]) };
        let area = Item { scrolls: true, clip: true, limit: Some(LimitSpec { share: 0.85, keep: 40.0, max: 0.0 }), ..col(vec![4]) };
        let mut items = vec![pane, list, foot, area, text(100.0, 1000.0, 15.0, 15.0)];
        solve(&mut items, 0, [300.0, 400.0]);
        let cap = 0.85f32 * 400.0 - 40.0;
        assert_eq!(items[3].out.h, cap);
        assert_eq!(items[2].out.h, cap + 20.0);
        assert_eq!(items[1].out.h, 400.0 - (cap + 20.0));
        items[4].intrinsic[1] = 60.0;
        solve(&mut items, 0, [300.0, 400.0]);
        assert_eq!(items[3].out.h, 60.0);
        items[4].intrinsic[1] = 1000.0;
        items[3].limit = Some(LimitSpec { share: 0.85, keep: 40.0, max: 132.0 });
        solve(&mut items, 0, [300.0, 400.0]);
        assert_eq!(items[3].out.h, 132.0);
    }

    /// The root's 32-point top inset pads a child at the top by 32 and one already below it by nothing.
    #[test]
    fn safe_area_padding_pads_only_what_reaches_into_the_inset() {
        let root = Item { safe: [0.0, 0.0, 32.0, 0.0], flex: Flex { axis: FlexAxis::Column, ..Flex::default() }, ..item([Sizing::GROW, Sizing::GROW], vec![1, 3]) };
        let top = Item { safe_pad: [false, false, true, false], ..item([Sizing::GROW, Sizing::FIT], vec![2]) };
        let below = Item { safe_pad: [false, false, true, false], ..item([Sizing::GROW, Sizing::FIT], vec![4]) };
        let mut items = vec![root, top, text(50.0, 16.0, 12.0, 12.0), below, text(50.0, 16.0, 12.0, 12.0)];
        solve(&mut items, 0, [400.0, 300.0]);
        assert_eq!(items[2].out.y, 32.0);
        assert_eq!(items[1].out.h, 48.0);
        assert_eq!(items[4].out.y, 48.0);
    }
}
