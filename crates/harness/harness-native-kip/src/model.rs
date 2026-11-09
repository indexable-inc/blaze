//! den's voxel text format (`.voxels`) and the mesh made from it.
//!
//! A character is a few rigid parts, each a set of unit cells in its own space, hung on a parent at a pivot. The file
//! says it in lines a review can read, built from shapes rather than cell by cell:
//!
//! ```text
//! voxels 1
//! ink body #8ea2ff                         // a named colour; the scene's `MascotInk` of that name replaces it
//! part body parent root at 0 1 0           // pivot in the parent's cells; `root` is the point between the feet
//!   rest 0 0 4                             // rest rotation, degrees, X then Y then Z
//!   fill ellipsoid 0 4.5 0  4.6 4.9 3.8 body
//!   paint box -2 2 2  1 4 3 belly          // recolours cells already there
//!   cut box -3 6 1  -2 7 2                 // takes cells out
//!   cell 0 9 0 tip
//! ```
//!
//! Cell (x, y, z) of a part spans x..x+1 (and so on) from its pivot; a shape takes the cells whose centres it holds, a
//! box its cells inclusive. x is right, y up, z toward the viewer. `//` starts a comment.
//!
//! The mesh is every face of a cell that has no neighbour in its own part, with each corner's ambient occlusion from the
//! three cells in front of it (Lysenko's voxel AO, fed1eb428d:docs/research/mascot.md): two triangles per face, split along the
//! brighter diagonal so the shade stays in the corners. Faces are not merged (greedy meshing would merge faces whose
//! corners shade differently, and a character is a couple of thousand triangles anyway).
use bytemuck::{Pod, Zeroable};
use std::collections::BTreeMap;

/// The most inks and parts a character has: what the shader's tables hold.
pub const MAX_INKS: usize = 16;
pub const MAX_PARTS: usize = 24;

/// One rigid part: its cells, its pivot in its parent and its rest rotation.
#[derive(Clone, Debug, PartialEq)]
pub struct Part {
    pub name: String,
    /// None: hung on the root (the ground point between the feet).
    pub parent: Option<usize>,
    /// The pivot, in the parent's cells.
    pub at: [f32; 3],
    /// Rest rotation, radians, about X then Y then Z.
    pub rest: [f32; 3],
    /// Filled cells and their ink.
    pub cells: BTreeMap<[i32; 3], u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    /// Each ink's name and its colour as the file gives it (sRGB, 0 to 1).
    pub inks: Vec<(String, [f32; 3])>,
    /// Parents come before their children.
    pub parts: Vec<Part>,
}

/// One corner of a face: `pos` the corner in its part's space and the part (`w`), `info` its normal (0 +x, 1 -x, 2 +y,
/// 3 -y, 4 +z, 5 -z), its ink, its occlusion (0 darkest to 3 open) and which corner of the face it is (0 to 3).
/// Byte for byte `MascotVertex` in harness-native-shaders' `mascot`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct Vertex {
    pub pos: [f32; 4],
    pub info: [u32; 4],
}

/// Each face direction: its normal, then the two axes across it (u x v = n, both positive).
const FACES: [([i32; 3], [i32; 3], [i32; 3]); 6] = [
    ([1, 0, 0], [0, 1, 0], [0, 0, 1]),
    ([-1, 0, 0], [0, 0, 1], [0, 1, 0]),
    ([0, 1, 0], [0, 0, 1], [1, 0, 0]),
    ([0, -1, 0], [1, 0, 0], [0, 0, 1]),
    ([0, 0, 1], [1, 0, 0], [0, 1, 0]),
    ([0, 0, -1], [0, 1, 0], [1, 0, 0]),
];

fn add(a: [i32; 3], b: [i32; 3]) -> [i32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn neg(a: [i32; 3]) -> [i32; 3] {
    [-a[0], -a[1], -a[2]]
}

/// A shape of the file, in a part's cells.
enum Shape {
    Ellipsoid([f32; 3], [f32; 3]),
    Box([i32; 3], [i32; 3]),
}

impl Shape {
    /// Every cell it takes.
    fn cells(&self) -> Vec<[i32; 3]> {
        let mut out = Vec::new();
        match *self {
            Shape::Ellipsoid(c, r) => {
                let lo = |i: usize| (c[i] - r[i] - 1.0).floor() as i32;
                let hi = |i: usize| (c[i] + r[i] + 1.0).ceil() as i32;
                for x in lo(0)..=hi(0) {
                    for y in lo(1)..=hi(1) {
                        for z in lo(2)..=hi(2) {
                            let d = |v: i32, i: usize| (v as f32 + 0.5 - c[i]) / r[i].max(1e-3);
                            if d(x, 0).powi(2) + d(y, 1).powi(2) + d(z, 2).powi(2) <= 1.0 + 1e-4 {
                                out.push([x, y, z]);
                            }
                        }
                    }
                }
            }
            Shape::Box(a, b) => {
                for x in a[0].min(b[0])..=a[0].max(b[0]) {
                    for y in a[1].min(b[1])..=a[1].max(b[1]) {
                        for z in a[2].min(b[2])..=a[2].max(b[2]) {
                            out.push([x, y, z]);
                        }
                    }
                }
            }
        }
        out
    }
}

impl Model {
    /// Read a `.voxels` file. The error names the line and what is wrong with it.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut inks: Vec<(String, [f32; 3])> = Vec::new();
        let mut parts: Vec<Part> = Vec::new();
        let mut header = false;
        for (n, raw) in text.lines().enumerate() {
            let line = raw.split("//").next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let at = |why: String| format!("line {}: {why}", n + 1);
            let words: Vec<&str> = line.split_whitespace().collect();
            let num = |i: usize| -> Result<f32, String> {
                let w = words.get(i).ok_or_else(|| at(format!("`{}` needs more numbers", words[0])))?;
                w.parse::<f32>().map_err(|_| at(format!("`{w}` is not a number")))
            };
            let int = |i: usize| -> Result<i32, String> {
                let w = words.get(i).ok_or_else(|| at(format!("`{}` needs more numbers", words[0])))?;
                w.parse::<i32>().map_err(|_| at(format!("`{w}` is not a whole number of cells")))
            };
            if !header {
                if words != ["voxels", "1"] {
                    return Err(at("a voxels file starts with `voxels 1`".into()));
                }
                header = true;
                continue;
            }
            let ink_of =
                |name: &str, inks: &[(String, [f32; 3])]| -> Result<u8, String> { inks.iter().position(|(i, _)| i == name).map(|i| i as u8).ok_or_else(|| at(format!("no ink is called `{name}`"))) };
            match words[0] {
                "ink" => {
                    let (Some(name), Some(hex)) = (words.get(1), words.get(2)) else { return Err(at("`ink` is a name and a #rrggbb colour".into())) };
                    let rgb = hex_colour(hex).ok_or_else(|| at(format!("`{hex}` is not a #rrggbb colour")))?;
                    if inks.iter().any(|(i, _)| i == name) {
                        return Err(at(format!("two inks are called `{name}`")));
                    }
                    if inks.len() == MAX_INKS {
                        return Err(at(format!("a character has at most {MAX_INKS} inks")));
                    }
                    inks.push(((*name).to_owned(), rgb));
                }
                "part" => {
                    let name = words.get(1).ok_or_else(|| at("`part` needs a name".into()))?;
                    if parts.iter().any(|p| p.name == *name) || *name == "root" {
                        return Err(at(format!("two parts are called `{name}`")));
                    }
                    if parts.len() == MAX_PARTS {
                        return Err(at(format!("a character has at most {MAX_PARTS} parts")));
                    }
                    let (mut parent, mut pivot, mut i) = (None, [0.0; 3], 2);
                    while i < words.len() {
                        match words[i] {
                            "parent" => {
                                let p = words.get(i + 1).ok_or_else(|| at("`parent` needs a part".into()))?;
                                parent = if *p == "root" { None } else { Some(parts.iter().position(|q| q.name == *p).ok_or_else(|| at(format!("`{p}` is not a part above this one")))?) };
                                i += 2;
                            }
                            "at" => {
                                pivot = [num(i + 1)?, num(i + 2)?, num(i + 3)?];
                                i += 4;
                            }
                            w => return Err(at(format!("`{w}`: a part line is `part <name> parent <part> at <x> <y> <z>`"))),
                        }
                    }
                    parts.push(Part { name: (*name).to_owned(), parent, at: pivot, rest: [0.0; 3], cells: BTreeMap::new() });
                }
                "rest" => {
                    let p = parts.last_mut().ok_or_else(|| at("`rest` belongs to a part".into()))?;
                    p.rest = [num(1)?.to_radians(), num(2)?.to_radians(), num(3)?.to_radians()];
                }
                op @ ("fill" | "paint" | "cut" | "cell") => {
                    let (shape, ink_word) = if op == "cell" {
                        let c = [int(1)?, int(2)?, int(3)?];
                        (Shape::Box(c, c), words.get(4))
                    } else {
                        match words.get(1).copied() {
                            Some("ellipsoid") => (Shape::Ellipsoid([num(2)?, num(3)?, num(4)?], [num(5)?, num(6)?, num(7)?]), words.get(8)),
                            Some("box") => (Shape::Box([int(2)?, int(3)?, int(4)?], [int(5)?, int(6)?, int(7)?]), words.get(8)),
                            _ => return Err(at(format!("`{op}` takes `ellipsoid` or `box`"))),
                        }
                    };
                    let ink = match (op, ink_word) {
                        ("cut", _) => None,
                        (_, Some(w)) => Some(ink_of(w, &inks)?),
                        (_, None) => return Err(at(format!("`{op}` needs an ink"))),
                    };
                    let part = parts.last_mut().ok_or_else(|| at(format!("`{op}` belongs to a part")))?;
                    for c in shape.cells() {
                        match (op, ink) {
                            ("cut", _) => {
                                part.cells.remove(&c);
                            }
                            ("paint", Some(i)) => {
                                if let Some(v) = part.cells.get_mut(&c) {
                                    *v = i;
                                }
                            }
                            (_, Some(i)) => {
                                part.cells.insert(c, i);
                            }
                            _ => {}
                        }
                    }
                }
                w => return Err(at(format!("`{w}` is not a line of a voxels file (ink, part, rest, fill, paint, cut, cell)"))),
            }
        }
        if !header {
            return Err("a voxels file starts with `voxels 1`".into());
        }
        if parts.iter().all(|p| p.cells.is_empty()) {
            return Err("the character has no cells".into());
        }
        Ok(Self { inks, parts })
    }

    /// How many cells it has.
    #[must_use]
    pub fn cell_count(&self) -> usize {
        self.parts.iter().map(|p| p.cells.len()).sum()
    }

    /// Every visible face of every part, as two triangles each (six corners, a triangle list).
    #[must_use]
    pub fn mesh(&self) -> Vec<Vertex> {
        let mut out = Vec::new();
        for (pi, part) in self.parts.iter().enumerate() {
            let filled = |c: [i32; 3]| part.cells.contains_key(&c);
            for (&c, &ink) in &part.cells {
                for (dir, &(n, u, v)) in FACES.iter().enumerate() {
                    if filled(add(c, n)) {
                        continue;
                    }
                    let front = add(c, n);
                    let base = [c[0] + n[0].max(0), c[1] + n[1].max(0), c[2] + n[2].max(0)];
                    let corner = |a: i32, b: i32| -> Vertex {
                        let su = if a == 1 { u } else { neg(u) };
                        let sv = if b == 1 { v } else { neg(v) };
                        let (s1, s2, k) = (filled(add(front, su)), filled(add(front, sv)), filled(add(add(front, su), sv)));
                        let ao = if s1 && s2 { 0 } else { 3 - u32::from(s1) - u32::from(s2) - u32::from(k) };
                        let p = [base[0] + u[0] * a + v[0] * b, base[1] + u[1] * a + v[1] * b, base[2] + u[2] * a + v[2] * b];
                        let which = [[0, 3], [1, 2]][a as usize][b as usize];
                        Vertex { pos: [p[0] as f32, p[1] as f32, p[2] as f32, pi as f32], info: [dir as u32, u32::from(ink), ao, which] }
                    };
                    let q = [corner(0, 0), corner(1, 0), corner(1, 1), corner(0, 1)];
                    let ao = |i: usize| q[i].info[2];
                    // Split along the brighter diagonal, so a dark corner stays a corner.
                    let tris = if ao(0) + ao(2) >= ao(1) + ao(3) { [0, 1, 2, 0, 2, 3] } else { [1, 2, 3, 1, 3, 0] };
                    out.extend(tris.iter().map(|&i| q[i]));
                }
            }
        }
        out
    }
}

/// `#rrggbb` as sRGB 0 to 1.
fn hex_colour(s: &str) -> Option<[f32; 3]> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok().map(|b| f32::from(b) / 255.0);
    Some([byte(0)?, byte(2)?, byte(4)?])
}
