//! A cell's drawing: `<Canvas>` (or SVG markup, which the kernel sends as the same elements) and its shapes. The tree under
//! a Canvas is flattened here: each shape is one `hn.GenShape` laid over the canvas's whole box, its SVG path data in the
//! canvas's user units, the transforms and opacities of the Groups round it folded into its own, drawn by the host's path
//! pass (harness-native-render vector.rs, the `Vector` paint). Inks are tones and series slots (`hn.GenTone`), never
//! colours. Den's Canvas (den/repl/src/ui/types.ts `CanvasProps`, `ShapeProps`), drawn with harness's primitives.
use super::*;
use harness_native_ui_layout_api::Aspect;
use harness_native_ui_look_api::{Rgba, Stroke};
use harness_native_ui_text_api::{Vector, VectorFit, VectorText};

/// The most shapes one Canvas draws (each is an entity; the element cap counts them too).
const MAX_SHAPES: usize = 400;
/// The most points a Polyline takes.
pub(super) const MAX_POINTS: usize = 2000;

/// An affine map, SVG's `matrix(a b c d e f)`: (x, y) to (a x + c y + e, b x + d y + f).
pub(super) type Affine = [f32; 6];
pub(super) const IDENTITY: Affine = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// `outer` after `inner`: a point goes through `inner`, then `outer` (a Group's transform round its child's).
pub(super) fn compose(p: Affine, c: Affine) -> Affine {
    [
        p[0] * c[0] + p[2] * c[1],
        p[1] * c[0] + p[3] * c[1],
        p[0] * c[2] + p[2] * c[3],
        p[1] * c[2] + p[3] * c[3],
        p[0] * c[4] + p[2] * c[5] + p[4],
        p[1] * c[4] + p[3] * c[5] + p[5],
    ]
}

/// One shape as the painter draws it: path data `d` (or none, for words alone) in user units, mapped by `m` then the view
/// box `view` fitted in the node's box less `inset` (l, t, r, b), in `fill` and `stroke` (None: not drawn).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Shape {
    pub d: String,
    pub view: [f32; 4],
    pub fit: VectorFit,
    pub m: Affine,
    pub inset: [f32; 4],
    pub fill: Option<Rgba2>,
    pub stroke: Option<Rgba2>,
    pub width: f32,
    /// The stroke's width and the words' size in points, whatever the scale (a chart's lines and labels).
    pub fixed: bool,
    pub even_odd: bool,
    pub text: Option<Words>,
}

/// An ink per appearance.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Rgba2 {
    pub dark: Rgba,
    pub light: Rgba,
}

/// SVG `<text>`: words at a point, starting at, centred on or ending at it (`anchor` 0, 1, 2), `size` user units tall.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Words {
    pub s: String,
    pub x: f32,
    pub y: f32,
    pub anchor: u8,
    pub size: f32,
    pub bold: bool,
}

impl Shape {
    pub(super) fn new(d: String, view: [f32; 4]) -> Self {
        Self {
            d,
            view,
            fit: VectorFit::Meet,
            m: IDENTITY,
            inset: [0.0; 4],
            fill: None,
            stroke: None,
            width: 1.0,
            fixed: false,
            even_odd: false,
            text: None,
        }
    }
}

/// Lay shape `s` onto its `hn.GenShape` entity, writing only what changed.
pub(super) fn put_shape(v: View<'_>, s: &Shape) {
    let m = if s.m == IDENTITY { [0.0; 6] } else { s.m };
    // A transform whose first four are all 0 reads as none; nudge one so a shape scaled to nothing stays nothing.
    let m = if m[..4].iter().all(|x| *x == 0.0) && s.m != IDENTITY { [1e-9, 0.0, 0.0, 1e-9, m[4], m[5]] } else { m };
    let want = Vector {
        d: s.d.as_str().into(),
        view_x: s.view[0],
        view_y: s.view[1],
        view_w: s.view[2],
        view_h: s.view[3],
        fit: s.fit,
        ma: m[0],
        mb: m[1],
        mc: m[2],
        md: m[3],
        me: m[4],
        mf: m[5],
        inset_l: s.inset[0],
        inset_t: s.inset[1],
        inset_r: s.inset[2],
        inset_b: s.inset[3],
        stroke_w: s.width,
        stroke_fixed: s.fixed,
        even_odd: s.even_odd,
    };
    let e = v.entity();
    if e.try_cloned::<&Vector>().as_ref() != Some(&want) {
        v.set(want);
    }
    let clear = Rgba2 { dark: Rgba::default(), light: Rgba::default() };
    let f = s.fill.unwrap_or(clear);
    let fill = Fill { dark: f.dark, light: f.light };
    if e.try_cloned::<&Fill>() != Some(fill) {
        v.set(fill);
    }
    let k = s.stroke.unwrap_or(clear);
    let stroke = Stroke { dark: k.dark, light: k.light };
    if e.try_cloned::<&Stroke>() != Some(stroke) {
        v.set(stroke);
    }
    match &s.text {
        Some(t) => {
            let want = VectorText { s: t.s.as_str().into(), x: t.x, y: t.y, anchor: t.anchor, size: t.size, bold: t.bold };
            if e.try_cloned::<&VectorText>().as_ref() != Some(&want) {
                v.set(want);
            }
        }
        None => {
            if e.has(VectorText::id()) {
                e.remove(VectorText::id());
            }
        }
    }
}

/// The ink a tone or series slot names (`hn.GenTone.<name>`), at `opacity`; Ok(None) for `none`. Err names an ink the
/// scene has not.
pub(super) fn ink(world: WorldRef<'_>, name: &str, opacity: f32) -> Result<Option<Rgba2>, String> {
    if name == "none" || name == "transparent" {
        return Ok(None);
    }
    let name = if name.is_empty() || name == "currentColor" { "ink" } else { name };
    let Some(f) = view::prefab(world, &format!("hn.GenTone.{name}")).and_then(|p| p.try_cloned::<&Fill>()) else {
        return Err(format!(
            "ink \u{201C}{}\u{201D} is not one: inks are tones (ink muted strong error accent success warning), faint, \
             series1 to series8 and none",
            name.chars().take(32).collect::<String>()
        ));
    };
    let a = opacity.clamp(0.0, 1.0);
    let k = |c: Rgba| Rgba { a: c.a * a, ..c };
    Ok(Some(Rgba2 { dark: k(f.dark), light: k(f.light) }))
}

fn num(node: &Value, name: &str) -> Option<f32> {
    prop(node, name).as_f64().filter(|v| v.is_finite()).map(|v| v as f32)
}

/// A transform prop: `[a, b, c, d, e, f]`.
fn matrix(node: &Value, name: &str) -> Result<Affine, String> {
    match prop(node, name) {
        Value::Null => Ok(IDENTITY),
        Value::Array(a) if a.len() == 6 && a.iter().all(|x| x.as_f64().is_some_and(f64::is_finite)) => {
            let n: Vec<f32> = a.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect();
            Ok([n[0], n[1], n[2], n[3], n[4], n[5]])
        }
        v => Err(format!("{name}={v} is not a transform: six numbers [a, b, c, d, e, f]")),
    }
}

/// A Group's own move: `transform`, then `x`, `y`, `scale` and `rotate` (degrees) as den's Group takes them.
fn group_matrix(node: &Value) -> Result<Affine, String> {
    let own = matrix(node, "transform")?;
    let s = num(node, "scale").unwrap_or(1.0);
    let (sn, cs) = num(node, "rotate").unwrap_or(0.0).to_radians().sin_cos();
    let place = [cs * s, sn * s, -sn * s, cs * s, num(node, "x").unwrap_or(0.0), num(node, "y").unwrap_or(0.0)];
    Ok(compose(place, own))
}

/// A Canvas's view box: `[x, y, w, h]` or `"x y w h"`; None when not given.
fn view_box(node: &Value) -> Result<Option<[f32; 4]>, String> {
    let nums: Vec<f32> = match prop(node, "view") {
        Value::Null => return Ok(None),
        Value::String(s) => s.split([' ', ',']).filter(|t| !t.is_empty()).filter_map(|t| t.parse().ok()).collect(),
        Value::Array(a) => a.iter().filter_map(Value::as_f64).map(|x| x as f32).collect(),
        _ => Vec::new(),
    };
    match nums[..] {
        [x, y, w, h] if w > 0.0 && h > 0.0 && [x, y, w, h].iter().all(|v| v.is_finite()) => Ok(Some([x, y, w, h])),
        _ => Err(format!("view={} is not a view box: four numbers, x y width height, the last two above 0", prop(node, "view"))),
    }
}

/// A shape element's path data in its own user units, and whether it is an open line (stroked, not filled, when its
/// inks are not said).
fn path_of(node: &Value) -> Result<(String, bool), String> {
    let kind = node["type"].as_str().unwrap_or("");
    let n = |name: &str| num(node, name).unwrap_or(0.0);
    let need = |name: &str| num(node, name).ok_or_else(|| format!("<{kind}> needs {name}, a number"));
    Ok(match kind {
        "Path" => {
            let d = words(node, "d");
            // The painter reads it again; here it is read once to say what is wrong where the agent can see it.
            harness_native_render::vector::segments(&d).map_err(|e| format!("<Path d> {e}"))?;
            (d, false)
        }
        "Rect" => {
            let (x, y, w, h) = (n("x"), n("y"), need("width")?, need("height")?);
            let r = n("radius").max(n("rx")).min(w * 0.5).min(h * 0.5).max(0.0);
            if r > 0.0 {
                let d = format!(
                    "M{} {y}H{}A{r} {r} 0 0 1 {} {}V{}A{r} {r} 0 0 1 {} {}H{}A{r} {r} 0 0 1 {x} {}V{}A{r} {r} 0 0 1 {} {y}Z",
                    x + r, x + w - r, x + w, y + r, y + h - r, x + w - r, y + h, x + r, y + h - r, y + r, x + r
                );
                (d, false)
            } else {
                (format!("M{x} {y}H{}V{}H{x}Z", x + w, y + h), false)
            }
        }
        "Circle" => {
            let (cx, cy) = (need("cx")?, need("cy")?);
            let r = n("r");
            let (rx, ry) = (num(node, "rx").unwrap_or(r), num(node, "ry").unwrap_or(r));
            if rx <= 0.0 || ry <= 0.0 {
                return Err(format!("<{kind}> needs r (or rx and ry) above 0"));
            }
            (format!("M{} {cy}A{rx} {ry} 0 1 0 {} {cy}A{rx} {ry} 0 1 0 {} {cy}Z", cx - rx, cx + rx, cx - rx), false)
        }
        "Line" => (format!("M{} {}L{} {}", n("x1"), n("y1"), n("x2"), n("y2")), true),
        "Polyline" => {
            let pts: Vec<f32> = match prop(node, "points") {
                Value::String(s) => s.split([' ', ',']).filter(|t| !t.is_empty()).filter_map(|t| t.parse().ok()).collect(),
                Value::Array(a) => a
                    .iter()
                    .flat_map(|p| match p {
                        Value::Array(xy) => xy.iter().filter_map(Value::as_f64).map(|x| x as f32).collect::<Vec<_>>(),
                        x => x.as_f64().map(|x| x as f32).into_iter().collect(),
                    })
                    .collect(),
                _ => Vec::new(),
            };
            if pts.len() < 4 || pts.len() % 2 != 0 {
                return Err(format!("<{kind}> points are {} numbers: it takes pairs, at least two points", pts.len()));
            }
            if pts.len() / 2 > MAX_POINTS {
                return Err(format!("<{kind}> has {} points: it draws at most {MAX_POINTS}", pts.len() / 2));
            }
            let closed = prop(node, "closed").as_bool().unwrap_or(false);
            let mut d = String::with_capacity(pts.len() * 8); // alloc: the path data the shape's Vector keeps
            for (i, p) in pts.chunks(2).enumerate() {
                d.push_str(&format!("{}{} {}", if i == 0 { 'M' } else { 'L' }, p[0], p[1]));
            }
            if closed {
                d.push('Z');
            }
            (d, !closed)
        }
        _ => unreachable!("path_of is called for shapes"),
    })
}

/// What a Canvas holds.
pub(super) const SHAPES: [&str; 7] = ["Group", "Path", "Rect", "Circle", "Line", "Polyline", "Text"];

/// The shapes under a Canvas, flattened: Groups folded into their children's transforms and opacities. Err names the
/// first element that is not a shape, or a shape's prop that is wrong.
fn flatten(
    world: WorldRef<'_>,
    nodes: &[Value],
    m: Affine,
    opacity: f32,
    view: [f32; 4],
    out: &mut Vec<Shape>,
    depth: usize,
) -> Result<(), String> {
    for node in nodes {
        let kind = node["type"].as_str().unwrap_or("");
        if !SHAPES.contains(&kind) {
            return Err(format!("<{kind}> inside a <Canvas>: a Canvas holds {}", SHAPES.join(", ")));
        }
        if out.len() >= MAX_SHAPES {
            return Err(format!("a Canvas draws at most {MAX_SHAPES} shapes"));
        }
        let at = |e: String| format!("<{kind}> {e}");
        let own = compose(m, matrix(node, "transform").map_err(at)?);
        let alpha = opacity * num(node, "opacity").unwrap_or(1.0).clamp(0.0, 1.0);
        if kind == "Group" {
            if depth >= MAX_DEPTH {
                return Err(format!("Groups nest at most {MAX_DEPTH} deep"));
            }
            let g = compose(m, group_matrix(node).map_err(at)?);
            let kids = node["children"].as_array().map_or(&[][..], Vec::as_slice);
            flatten(world, kids, g, alpha, view, out, depth + 1)?;
            continue;
        }
        let fill_op = alpha * num(node, "fillOpacity").unwrap_or(1.0);
        let stroke_op = alpha * num(node, "strokeOpacity").unwrap_or(1.0);
        let named = |p: &str| -> Option<String> { prop(node, p).as_str().map(str::to_owned) };
        if kind == "Text" {
            let mut s = Shape::new(String::new(), view);
            s.m = own;
            let anchor = match words(node, "anchor").as_str() {
                "middle" => 1,
                "end" => 2,
                _ => 0,
            };
            s.fill = ink(world, &named("fill").unwrap_or_default(), fill_op).map_err(at)?;
            s.text = Some(Words {
                s: words(node, "text"),
                x: num(node, "x").unwrap_or(0.0),
                y: num(node, "y").unwrap_or(0.0),
                anchor,
                size: num(node, "size").filter(|s| *s > 0.0).unwrap_or(12.0),
                bold: prop(node, "bold").as_bool().unwrap_or(false),
            });
            out.push(s);
            continue;
        }
        let (d, open) = path_of(node).map_err(|e| if e.starts_with('<') { e } else { at(e) })?;
        let mut s = Shape::new(d, view);
        s.m = own;
        let (fill, stroke) = (named("fill"), named("stroke"));
        // Den's rule: a shape is filled in the ink when its inks are not said, an open line stroked in it.
        let (fill, stroke) = match (fill, stroke) {
            (None, None) if open => (None, Some("ink".to_owned())),
            (None, None) => (Some("ink".to_owned()), None),
            (f, k) => (f, k),
        };
        s.fill = match fill {
            Some(f) => ink(world, &f, fill_op).map_err(at)?,
            None => None,
        };
        s.stroke = match stroke {
            Some(k) => ink(world, &k, stroke_op).map_err(at)?,
            None => None,
        };
        s.width = num(node, "strokeWidth").filter(|w| *w >= 0.0).unwrap_or(1.0);
        s.even_odd = words(node, "fillRule") == "evenodd";
        out.push(s);
    }
    Ok(())
}

/// A `<Canvas>`: its box (`width` and `height` points; else as wide as the room and as tall as its view box makes it) and
/// its shapes over it. Err (drawn in the failed tone) says what is wrong.
pub(super) fn canvas(v: View<'_>, node: &Value, b: &mut Budget<'_, '_>) -> Result<(), String> {
    let world = v.entity().world();
    let view = view_box(node)?;
    let (w, h) = (points(node, "width"), points(node, "height"));
    let size = match (w, h, view) {
        (Some(w), Some(h), _) => Size { w: Sizing::fixed(w), h: Sizing::fixed(h) },
        (Some(w), None, Some(vb)) => Size { w: Sizing::fixed(w), h: Sizing::fixed(w * vb[3] / vb[2]) },
        (None, Some(h), Some(vb)) => Size { w: Sizing::fixed(h * vb[2] / vb[3]), h: Sizing::fixed(h) },
        (Some(w), None, None) => Size { w: Sizing::fixed(w), h: Sizing::fixed(160.0) },
        (None, Some(h), None) => Size { w: Sizing::GROW, h: Sizing::fixed(h) },
        // A view box alone: drawn at one point a unit, up to the room's width, keeping its shape.
        (None, None, Some(vb)) => {
            let w = vb[2].clamp(16.0, 640.0);
            Size { w: Sizing { kind: SizeKind::Grow, value: 1.0, min: 0.0, max: w }, h: Sizing::FIT }
        }
        (None, None, None) => Size { w: Sizing::GROW, h: Sizing::fixed(160.0) },
    };
    if v.entity().try_cloned::<&Size>() != Some(size) {
        v.set(size);
    }
    // Kept to its view box's shape when only that sizes it.
    let aspect = match (w, h, view) {
        (None, None, Some(vb)) => Some(Aspect { ratio: vb[2] / vb[3] }),
        _ => None,
    };
    match aspect {
        Some(a) if v.entity().try_cloned::<&Aspect>() != Some(a) => {
            v.set(a);
        }
        None if v.entity().has(Aspect::id()) => {
            v.entity().remove(Aspect::id());
        }
        _ => {}
    }
    // No view box: one user unit a point, from the box's corner.
    let vb = view.unwrap_or([0.0; 4]);
    let kids = node["children"].as_array().map_or(&[][..], Vec::as_slice);
    let mut shapes = Vec::new(); // alloc: this frame's shapes of one canvas, laid onto its entities and dropped
    flatten(world, kids, IDENTITY, 1.0, vb, &mut shapes, 0)?;
    if shapes.len() > b.left {
        b.cut = true;
        shapes.truncate(b.left);
    }
    b.left -= shapes.len();
    tip(v, &words(node, "tip"));
    view::for_each_of(v, shapes.iter().enumerate().map(|(i, s)| (format!("s{i}"), "hn.GenShape", s)), |sv, s, _| {
        put_shape(sv, s)
    });
    Ok(())
}

/// The words shown while the pointer rests on `v` (none: no tip, and it takes no hover).
pub(super) fn tip(v: View<'_>, s: &str) {
    use harness_native_ui_input_api::{Hoverable, Tooltip};
    let e = v.entity();
    if s.is_empty() {
        if e.has(Tooltip::id()) {
            e.remove(Tooltip::id());
        }
        return;
    }
    if e.try_cloned::<&Tooltip>().is_none_or(|t| t.text.as_str() != s) {
        v.set(Tooltip { text: s.into() });
    }
    if !e.has(Hoverable::id()) {
        e.add(Hoverable::id());
    }
}

/// What is wrong with a `<Canvas>` and its shapes, named, or None (genui.rs draws it in the failed tone instead).
pub(super) fn check(world: WorldRef<'_>, node: &Value) -> Option<String> {
    let vb = match view_box(node) {
        Ok(v) => v.unwrap_or([0.0; 4]),
        Err(e) => return Some(format!("<Canvas> {e}")),
    };
    let kids = node["children"].as_array().map_or(&[][..], Vec::as_slice);
    let mut scratch = Vec::new(); // alloc: the shapes read once to check them, dropped
    flatten(world, kids, IDENTITY, 1.0, vb, &mut scratch, 0).err()
}
