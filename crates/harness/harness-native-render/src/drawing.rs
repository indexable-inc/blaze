//! Multi-colour vector drawings: an SVG read once into layers, each a shape (vector.rs's segments) with its own fill and
//! stroke, drawn in its own colours by the path pass (a `PathInst` a fill, `LineInst`s a stroke) in document order, so a
//! drawing is as crisp at every zoom as a `Vector` and keeps the colours it was drawn in. The vendored set is the file
//! icons (icons/files: IntelliJ's New UI icons, see its NOTICE), named by file stem; a `ui.Art` node names one by its
//! `Drawing {name}`, and a file chip's icon is one (paint.rs). A dark appearance takes `<name>_dark` where there is one.
//! Which file kind is drawn with which is harness-native-ui-inline's files.rs.
//!
//! The reader takes the SVG these icons are written in and refuses the rest, naming it: `<svg viewBox>` with a `fill`
//! its shapes inherit; `<path d>`, `<rect x y width height rx ry>`, `<circle cx cy r>`, `<ellipse cx cy rx ry>`, `<line
//! x1 y1 x2 y2>`, `<polyline points>`, `<polygon points>`; paint `fill`, `stroke` (`#rgb`, `#rrggbb`, `none`, `white`,
//! `black`), `stroke-width`, `fill-rule`, `opacity`, `fill-opacity`, `stroke-opacity`. Every stroke the path pass draws is
//! round at its ends and bends, so `stroke-linecap`, `stroke-linejoin` and `stroke-miterlimit` are read past (at these
//! icons' one-unit strokes a mitred corner and a round one differ by a fraction of a pixel); `clip-rule` matters only to
//! a clip. Groups, transforms, clips, masks, gradients and text are refused: none of the set uses them, and a file that
//! did would draw wrong, not merely plainer.
use std::sync::OnceLock;

use crate::draw::Draw;
use crate::pack::{LineInst, PathInst};
use crate::vector::Seg;

/// The most points one drawing flattens to (an icon takes a few hundred); past it the paint fails, named.
const MAX_POINTS: usize = 20_000;

/// A cubic's control distance for a quarter circle of radius 1.
const KAPPA: f64 = 0.552_284_749_830_793_4;

/// One shape of a drawing and its paint: `fill` with its rule (`even_odd`), then `stroke` (its ink, and its width in
/// user units) over it. Inks are straight sRGB with their opacity folded in.
#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    pub segs: Vec<Seg>,
    pub fill: Option<[f32; 4]>,
    pub even_odd: bool,
    pub stroke: Option<([f32; 4], f64)>,
}

/// A drawing: its view box (x, y, w, h, user units) and its layers, back to front.
#[derive(Clone, Debug, PartialEq)]
pub struct Art {
    pub view: [f64; 4],
    pub layers: Vec<Layer>,
}

macro_rules! files {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_str!(concat!("../icons/files/", $name, ".svg")))),*]
    };
}

/// The vendored drawings by name (the file's stem): `<name>_dark` is a name's dark-appearance variant, where it has one.
const SOURCES: &[(&str, &str)] = files![
    "archive", "archive_dark", "binaryData", "binaryData_dark", "c", "c_dark", "config", "config_dark", "cpp", "cpp_dark",
    "css", "css_dark", "csv", "csv_dark", "docker", "docker_dark", "editorConfig", "editorConfig_dark", "folder",
    "folder_dark", "folderGithub", "folderGithub_dark", "folderOpen", "folderOpen_dark", "font", "font_dark", "gitignore",
    "go", "go_dark", "graphql", "graphql_dark", "h", "h_dark", "html", "html_dark", "image", "image_dark", "java",
    "java_dark", "javaScript", "javaScript_dark", "json", "json_dark", "kotlin", "kotlin_dark", "lockFile",
    "lockFile_dark", "markdown", "markdown_dark", "nix", "nix_dark", "python", "python_dark", "ruby", "ruby_dark", "rust",
    "rust_dark", "shell", "shell_dark", "sql", "sql_dark", "swiftLang", "swiftLang_dark", "terraform", "terraform_dark",
    "text", "text_dark", "toml", "toml_dark", "typeScript", "typeScript_dark", "xml", "xml_dark", "yaml", "yaml_dark",
];

/// Every vendored drawing, read once. A file the reader refuses is a bug in the set: it panics naming the file and why.
fn all() -> &'static [(&'static str, Art)] {
    static ALL: OnceLock<Vec<(&'static str, Art)>> = OnceLock::new();
    ALL.get_or_init(|| {
        // alloc: the vendored drawings, read once for the process
        SOURCES
            .iter()
            .map(|&(name, svg)| (name, parse(svg).unwrap_or_else(|e| panic!("icons/files/{name}.svg: {e}"))))
            .collect()
    })
}

/// Drawing `name` in an appearance: its `_dark` variant when `dark` and it has one, else itself. None for a name the
/// set does not have.
pub fn get(name: &str, dark: bool) -> Option<&'static Art> {
    let find = |n: &str| all().iter().find(|(k, _)| *k == n).map(|(_, a)| a);
    let own = find(name)?;
    if dark && let Some(d) = all().iter().find(|(k, _)| k.strip_suffix("_dark") == Some(name)).map(|(_, a)| a) {
        return Some(d);
    }
    Some(own)
}

/// The names the set has, without their dark variants (an error lists them).
pub fn names() -> impl Iterator<Item = &'static str> {
    SOURCES.iter().map(|(n, _)| *n).filter(|n| !n.ends_with("_dark"))
}

impl Art {
    /// Draw it into `rect` (points: x, y, w, h), its view box fitted and centred (SVG's `xMidYMid meet`), at `alpha`,
    /// curves cut a quarter of a device pixel (`sc` pixels a point) off true at most. Err when it flattens to more
    /// points than [`MAX_POINTS`].
    pub fn emit(&self, d: &mut Draw, rect: [f32; 4], alpha: f32, sc: f32) -> Result<(), String> {
        let [vx, vy, vw, vh] = self.view;
        let (bw, bh) = (f64::from(rect[2].max(0.0)), f64::from(rect[3].max(0.0)));
        let s = (bw / vw).min(bh / vh);
        let ox = f64::from(rect[0]) + (bw - vw * s) * 0.5 - vx * s;
        let oy = f64::from(rect[1]) + (bh - vh * s) * 0.5 - vy * s;
        let to = |p: [f64; 2]| [(ox + p[0] * s) as f32, (oy + p[1] * s) as f32];
        let tol = 0.25 / sc.max(0.1);
        let mut budget = MAX_POINTS;
        for layer in &self.layers {
            let (rings, cut) = crate::vector::flatten(&layer.segs, to, tol, budget);
            if cut {
                return Err(format!("a drawing flattens to more than {MAX_POINTS} points"));
            }
            budget -= rings.iter().map(|r| r.pts.len()).sum::<usize>();
            if let Some(ink) = layer.fill {
                // alloc: this layer's rings as plain polygons, for one frame
                let polys: Vec<Vec<[f32; 2]>> = rings.iter().map(|r| r.pts.clone()).collect();
                if let Some(inst) = PathInst::fill(&polys, layer.even_odd, (ink, ink), alpha, &mut d.edges) {
                    d.path(inst);
                }
            }
            if let Some((ink, w)) = layer.stroke {
                let w = (w * s) as f32;
                for r in &rings {
                    let mut pts = r.pts.clone(); // alloc: one ring's stroke points, for one frame
                    if r.closed && pts.first() != pts.last() {
                        pts.push(pts[0]);
                    }
                    for l in LineInst::stroke(&pts, w, (ink, ink), alpha) {
                        d.line(l);
                    }
                }
            }
        }
        Ok(())
    }
}

/// Read `svg` (the subset the module doc lists) into a drawing. Err names the element or attribute it refuses.
pub fn parse(svg: &str) -> Result<Art, String> {
    let mut view = None;
    // The fill a shape that says none inherits from the `<svg>` (SVG's initial fill is black).
    let mut root_fill = Some([0.0, 0.0, 0.0, 1.0]);
    let mut layers = Vec::new(); // alloc: the drawing's layers, kept with it for the process
    let mut rest = svg;
    while let Some(at) = rest.find('<') {
        rest = &rest[at..];
        // A comment, a declaration or a closing tag: past it.
        if let Some((open, close)) = [("<!--", "-->"), ("<?", "?>"), ("</", ">")].into_iter().find(|(o, _)| rest.starts_with(o)) {
            let end = rest.find(close).ok_or_else(|| format!("unclosed {open}"))?;
            rest = &rest[end + close.len()..];
            continue;
        }
        let end = tag_end(rest).ok_or("an unclosed tag")?;
        let (tag, after) = (&rest[1..end], &rest[end + 1..]);
        rest = after;
        let tag = tag.strip_suffix('/').unwrap_or(tag);
        let name_end = tag.find(|c: char| c.is_ascii_whitespace()).unwrap_or(tag.len());
        let (name, attrs) = (&tag[..name_end], attributes(&tag[name_end..])?);
        let attr = |k: &str| attrs.iter().find(|(n, _)| *n == k).map(|(_, v)| *v);
        let num = |k: &str| -> Result<f64, String> {
            attr(k).map_or(Ok(0.0), |v| v.trim().parse::<f64>().map_err(|_| format!("<{name}> {k}=\"{v}\" is not a number")))
        };
        if name == "title" {
            // A title's words are not drawn: past them to its close.
            let close = rest.find("</title>").ok_or("<title> is not closed")?;
            rest = &rest[close + "</title>".len()..];
            continue;
        }
        for (k, v) in &attrs {
            if !KNOWN.contains(k) {
                return Err(format!("<{name}> {k}=\"{v}\": the drawing reader does not read {k}"));
            }
        }
        if name == "svg" {
            let v: Vec<f64> = attr("viewBox")
                .ok_or("<svg> has no viewBox")?
                .split(|c: char| c.is_ascii_whitespace() || c == ',')
                .filter(|t| !t.is_empty())
                .map(|t| t.parse::<f64>().map_err(|_| format!("<svg> viewBox has {t:?}, not a number")))
                .collect::<Result<_, _>>()?;
            let [x, y, w, h] = v[..] else { return Err(format!("<svg> viewBox has {} numbers, not 4", v.len())) };
            if w <= 0.0 || h <= 0.0 {
                return Err(format!("<svg> viewBox is {w} by {h}"));
            }
            view = Some([x, y, w, h]);
            if let Some(f) = attr("fill") {
                root_fill = colour(f)?;
            }
            continue;
        }
        let segs = match name {
            "path" => crate::vector::segments(attr("d").ok_or("<path> has no d")?).map_err(|e| format!("<path> d: {e}"))?,
            "rect" => {
                let (rx, ry) = (attr("rx").map(|_| num("rx")).transpose()?, attr("ry").map(|_| num("ry")).transpose()?);
                rect(num("x")?, num("y")?, num("width")?, num("height")?, rx.or(ry).unwrap_or(0.0), ry.or(rx).unwrap_or(0.0))
            }
            "circle" => ellipse(num("cx")?, num("cy")?, num("r")?, num("r")?),
            "ellipse" => ellipse(num("cx")?, num("cy")?, num("rx")?, num("ry")?),
            "line" => vec![Seg::Move([num("x1")?, num("y1")?]), Seg::Line([num("x2")?, num("y2")?])],
            "polyline" | "polygon" => {
                let n: Vec<f64> = attr("points")
                    .unwrap_or("")
                    .split(|c: char| c.is_ascii_whitespace() || c == ',')
                    .filter(|t| !t.is_empty())
                    .map(|t| t.parse::<f64>().map_err(|_| format!("<{name}> points has {t:?}, not a number")))
                    .collect::<Result<_, _>>()?;
                let mut segs: Vec<Seg> = n.chunks_exact(2).enumerate().map(|(i, p)| if i == 0 { Seg::Move([p[0], p[1]]) } else { Seg::Line([p[0], p[1]]) }).collect();
                if name == "polygon" {
                    segs.push(Seg::Close);
                }
                segs
            }
            other => return Err(format!("<{other}>: the drawing reader draws path, rect, circle, ellipse, line, polyline and polygon only")),
        };
        let opacity = attr("opacity").map(|_| num("opacity")).transpose()?.unwrap_or(1.0);
        let faded = |ink: Option<[f32; 4]>, k: &str| -> Result<Option<[f32; 4]>, String> {
            let a = attr(k).map(|_| num(k)).transpose()?.unwrap_or(1.0) * opacity;
            Ok(ink.map(|c| [c[0], c[1], c[2], c[3] * a as f32]).filter(|c| c[3] > 0.0))
        };
        let fill = match attr("fill") {
            Some(f) => colour(f)?,
            None => root_fill,
        };
        let stroke = attr("stroke").map(colour).transpose()?.flatten();
        let width = attr("stroke-width").map(|_| num("stroke-width")).transpose()?.unwrap_or(1.0);
        let even_odd = match attr("fill-rule") {
            None | Some("nonzero") => false,
            Some("evenodd") => true,
            Some(r) => return Err(format!("<{name}> fill-rule=\"{r}\" is neither nonzero nor evenodd")),
        };
        let stroke = faded(stroke, "stroke-opacity")?.filter(|_| width > 0.0).map(|c| (c, width));
        layers.push(Layer { segs, fill: faded(fill, "fill-opacity")?, even_odd, stroke });
    }
    Ok(Art { view: view.ok_or("no <svg> element")?, layers })
}

/// The attributes the reader reads, or reads past (the module doc says why).
const KNOWN: &[&str] = &[
    "xmlns", "width", "height", "viewBox", "role", "d", "x", "y", "rx", "ry", "cx", "cy", "r", "x1", "y1", "x2", "y2",
    "points", "fill", "stroke", "stroke-width", "fill-rule", "opacity", "fill-opacity", "stroke-opacity", "clip-rule",
    "stroke-linecap", "stroke-linejoin", "stroke-miterlimit", "color-fill-key",
];

/// Where the tag starting `s` (at its `<`) ends: its `>` outside any quoted value.
fn tag_end(s: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '>') => return Some(i),
            _ => {}
        }
    }
    None
}

/// A tag's `name="value"` pairs (either quote).
fn attributes(mut s: &str) -> Result<Vec<(&str, &str)>, String> {
    let mut out = Vec::new(); // alloc: one tag's attributes, while it is read
    loop {
        s = s.trim_start();
        if s.is_empty() {
            return Ok(out);
        }
        let eq = s.find('=').ok_or_else(|| format!("attribute {s:?} has no value"))?;
        let name = s[..eq].trim();
        let v = s[eq + 1..].trim_start();
        let q = v.chars().next().filter(|c| matches!(c, '"' | '\'')).ok_or_else(|| format!("attribute {name} is not quoted"))?;
        let end = v[1..].find(q).ok_or_else(|| format!("attribute {name} is not closed"))?;
        out.push((name, &v[1..1 + end]));
        s = &v[end + 2..];
    }
}

/// A paint: an ink (straight sRGB), or None for `none`.
fn colour(v: &str) -> Result<Option<[f32; 4]>, String> {
    let v = v.trim();
    let hex = |s: &str| u8::from_str_radix(s, 16).map(|b| f32::from(b) / 255.0).map_err(|_| format!("colour {v:?} is not hex"));
    Ok(Some(match v {
        "none" | "transparent" => return Ok(None),
        "white" => [1.0, 1.0, 1.0, 1.0],
        "black" => [0.0, 0.0, 0.0, 1.0],
        _ => match v.strip_prefix('#') {
            Some(h) if h.len() == 6 && h.is_ascii() => [hex(&h[0..2])?, hex(&h[2..4])?, hex(&h[4..6])?, 1.0],
            Some(h) if h.len() == 3 && h.is_ascii() => {
                let d = |i: usize| hex(&h[i..=i].repeat(2));
                [d(0)?, d(1)?, d(2)?, 1.0]
            }
            _ => return Err(format!("colour {v:?}: the drawing reader reads #rgb, #rrggbb, none, white and black")),
        },
    }))
}

/// A rectangle with corners of radii `rx` by `ry` (each at most half its side), clockwise from its top edge.
fn rect(x: f64, y: f64, w: f64, h: f64, rx: f64, ry: f64) -> Vec<Seg> {
    let (rx, ry) = (rx.clamp(0.0, w * 0.5), ry.clamp(0.0, h * 0.5));
    let (kx, ky) = (rx * KAPPA, ry * KAPPA);
    let (r, b) = (x + w, y + h);
    if rx == 0.0 || ry == 0.0 {
        return vec![Seg::Move([x, y]), Seg::Line([r, y]), Seg::Line([r, b]), Seg::Line([x, b]), Seg::Close];
    }
    vec![
        Seg::Move([x + rx, y]),
        Seg::Line([r - rx, y]),
        Seg::Cubic([r - rx + kx, y], [r, y + ry - ky], [r, y + ry]),
        Seg::Line([r, b - ry]),
        Seg::Cubic([r, b - ry + ky], [r - rx + kx, b], [r - rx, b]),
        Seg::Line([x + rx, b]),
        Seg::Cubic([x + rx - kx, b], [x, b - ry + ky], [x, b - ry]),
        Seg::Line([x, y + ry]),
        Seg::Cubic([x, y + ry - ky], [x + rx - kx, y], [x + rx, y]),
        Seg::Close,
    ]
}

/// An ellipse about `(cx, cy)`, four quarter cubics clockwise from its right.
fn ellipse(cx: f64, cy: f64, rx: f64, ry: f64) -> Vec<Seg> {
    let (kx, ky) = (rx * KAPPA, ry * KAPPA);
    vec![
        Seg::Move([cx + rx, cy]),
        Seg::Cubic([cx + rx, cy + ky], [cx + kx, cy + ry], [cx, cy + ry]),
        Seg::Cubic([cx - kx, cy + ry], [cx - rx, cy + ky], [cx - rx, cy]),
        Seg::Cubic([cx - rx, cy - ky], [cx - kx, cy - ry], [cx, cy - ry]),
        Seg::Cubic([cx + kx, cy - ry], [cx + rx, cy - ky], [cx + rx, cy]),
        Seg::Close,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_drawing_reads_and_draws_on_its_grid() {
        for &(name, _) in SOURCES {
            let art = get(name, false).unwrap_or_else(|| panic!("{name}"));
            assert!(!art.layers.is_empty(), "{name} draws something");
            let mut d = Draw::default();
            art.emit(&mut d, [0.0, 0.0, 16.0, 16.0], 1.0, 2.0).unwrap();
            assert!(!d.paths.is_empty() || !d.lines.is_empty(), "{name} paints");
            // All of it lands in its box (a stroke's half width past it at most).
            for e in &d.edges {
                assert!(e.iter().all(|v| (-1.0..=17.0).contains(v)), "{name}: edge {e:?} is off its box");
            }
        }
    }

    #[test]
    fn every_file_kind_is_drawn_by_a_drawing_the_set_has() {
        for (kind, name) in harness_native_ui_inline::files::KINDS {
            assert!(get(name, false).is_some(), "file kind {kind} names drawing {name}, which icons/files lacks");
        }
    }

    #[test]
    fn a_dark_appearance_takes_the_dark_variant_where_there_is_one() {
        assert_ne!(get("rust", true), get("rust", false));
        assert_eq!(get("gitignore", true), get("gitignore", false), "the light one where there is no dark one");
        assert!(get("rust_dark", false).is_some() && get("no-such", false).is_none());
        assert!(names().all(|n| !n.ends_with("_dark")) && names().any(|n| n == "folder"));
    }

    #[test]
    fn shapes_inherit_the_root_fill_and_keep_their_own_paint() {
        let art = parse(
            r##"<!-- a comment's <tags> are skipped -->
            <svg width="16" height="16" viewBox="0 0 16 16" fill="none" xmlns="http://www.w3.org/2000/svg">
            <rect x="2.5" y="2.5" width="11" height="11" rx="1.5" fill="#F5F8FE" stroke="#3574F0"/>
            <circle cx="10" cy="6" r="1.5" stroke="#36F" stroke-width="1.2"/>
            <path fill-rule="evenodd" d="M0 0H4V4Z" fill="white" opacity="0.5"/>
            </svg>"##,
        )
        .unwrap();
        assert_eq!(art.view, [0.0, 0.0, 16.0, 16.0]);
        let [rect, circle, path] = &art.layers[..] else { panic!("three layers: {:?}", art.layers.len()) };
        assert_eq!(rect.fill, Some([245.0 / 255.0, 248.0 / 255.0, 254.0 / 255.0, 1.0]));
        assert_eq!(rect.stroke.map(|s| s.1), Some(1.0), "a stroke is one unit unless it says");
        assert_eq!(circle.fill, None, "the root's fill=none is inherited");
        assert_eq!(circle.stroke, Some(([0.2, 0.4, 1.0, 1.0], 1.2)));
        assert!(path.even_odd && path.stroke.is_none());
        assert_eq!(path.fill, Some([1.0, 1.0, 1.0, 0.5]));
    }

    #[test]
    fn what_it_cannot_draw_is_refused_by_name() {
        let refused = |body: &str| parse(&format!(r#"<svg viewBox="0 0 16 16">{body}</svg>"#)).unwrap_err();
        assert!(refused(r#"<g clip-path="url(#c)"><path d="M0 0H1V1Z"/></g>"#).contains("clip-path"));
        assert!(refused(r#"<rect width="1" height="1" transform="rotate(90)"/>"#).contains("transform"));
        assert!(refused(r#"<g><path d="M0 0H1V1Z"/></g>"#).contains("<g>"));
        assert!(refused(r#"<path d="M0 0H1V1Z" fill="url(#grad)"/>"#).contains("url(#grad)"));
        assert!(parse(r#"<path d="M0 0H1V1Z"/>"#).unwrap_err().contains("no <svg>"));
    }

    #[test]
    fn a_rounded_rect_and_a_circle_have_their_true_areas() {
        // The shoelace sum of every ring. Chords sit inside the curve, so the flattened area is a little under the true
        // one (vector::flatten's own floor on its tolerance): within 0.1 of a 16-unit icon's, far under a pixel.
        let area = |segs: Vec<Seg>| {
            let (rings, _) = crate::vector::flatten(&segs, |p| [p[0] as f32, p[1] as f32], 0.001, 100_000);
            let ring = |pts: &[[f32; 2]]| {
                let n = pts.len();
                (0..n).map(|i| f64::from(pts[i][0] * pts[(i + 1) % n][1] - pts[(i + 1) % n][0] * pts[i][1])).sum::<f64>() * 0.5
            };
            rings.iter().map(|r| ring(&r.pts)).sum::<f64>().abs()
        };
        let pi = std::f64::consts::PI;
        assert!((area(rect(0.0, 0.0, 10.0, 6.0, 0.0, 0.0)) - 60.0).abs() < 1e-3);
        assert!((area(rect(0.0, 0.0, 10.0, 6.0, 2.0, 2.0)) - (60.0 - 16.0 + 4.0 * pi)).abs() < 0.1);
        assert!((area(ellipse(5.0, 5.0, 3.0, 2.0)) - 6.0 * pi).abs() < 0.1);
    }
}
