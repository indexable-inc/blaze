//! A binary glTF 2.0 (`.glb`) reader (Khronos, "glTF 2.0 Specification", 2021): parses the
//! GLB container, JSON chunk (with a bounded, zero-unsafe `#![no_std]` RFC 8259 parser), BIN
//! buffer views and accessors, float and `KHR_mesh_quantization` meshes (including `.mesh`
//! dequantization child nodes, `TEXCOORD_0`, `TANGENT`, and multi-role primitives), node
//! hierarchy and transforms, PBR materials with texture bindings, LODs, mass properties, and
//! embedded PNG images decoded via [`crate::png::decode_png`].

#![blaze::sans_io]

use blaze_cad_mesh::geom::{P3, add, cross, narrow, plus_zero, plus_zeros, sub, unit, wide};
use blaze_cad_mesh::{MaterialRole, Mesh, MeshVecs, PartId, ShapeError};
use blaze_int::{Int, idx, len32};
use blaze_own::{Alloc, AllocFailure, String, Vec};
use blaze_units::Length;

use crate::Limits;
use crate::png::{PngError, PngKind, decode_png};
use crate::scene::{
    Clearcoat, MassFacts, Material, MaterialTextures, Matrix, Transform, Trs, Up, unturn, unturn_extent, unturn_rotation,
    unturn_tensor,
};

type V<'a, T> = Vec<T, Alloc<'a>>;

const MAX_JSON_DEPTH: u32 = 64;
const BYTE: u64 = 5120;
const UNSIGNED_BYTE: u64 = 5121;
const SHORT: u64 = 5122;
const UNSIGNED_SHORT: u64 = 5123;
const UNSIGNED_INT: u64 = 5125;
const FLOAT: u64 = 5126;

/// Options for [`read_glb`].
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct GlbReadOpts<'a, 's> {
    /// Coordinate frame to return positions, normals, tangents, transforms and mass facts in.
    /// [`Up::Y`] preserves glTF's native Y-up frame; [`Up::Z`] turns back to CAD Z-up.
    pub up: Up,
    pub limits: Limits,
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

/// Why [`read_glb`] refused a `.glb` input.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum GlbReadError {
    TooShort { len: usize },
    BadMagic,
    UnsupportedVersion { version: u32 },
    LengthMismatch { declared: u32, actual: usize },
    BadChunk { why: &'static str },
    Json { offset: usize, why: &'static str },
    InvalidStructure { why: &'static str },
    OutOfRange { what: &'static str, index: u64, len: usize },
    TooManyParts { parts: usize, most: u32 },
    TooLarge { bytes: u64, most: u64 },
    BadAccessor { accessor: u32, why: &'static str },
    BadMesh { mesh: u32, why: &'static str },
    Shape(ShapeError),
    Png { image: u32, error: PngError },
    Alloc(AllocFailure),
}

impl From<AllocFailure> for GlbReadError {
    fn from(e: AllocFailure) -> GlbReadError {
        GlbReadError::Alloc(e)
    }
}

/// An embedded PNG image decoded to RGBA8 (greyscale PNGs expanded to `[g, g, g, 255]`).
#[derive(Debug)]
pub struct LoadedImage<'a> {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA8 (`width * height * 4` bytes), row-major top-to-bottom.
    pub rgba: Vec<u8, Alloc<'a>>,
}

/// A glTF 2.0 material with its PBR factors, extensions, and texture image indices.
#[derive(Debug)]
pub struct LoadedMaterial<'a> {
    pub name: String<Alloc<'a>>,
    pub base_colour: [f64; 4],
    pub metallic: f64,
    pub roughness: f64,
    pub emissive: [f64; 3],
    pub emissive_strength: Option<f64>,
    pub clearcoat: Option<Clearcoat>,
    pub double_sided: bool,
    /// Resolved indices into [`LoadedGlb::images`].
    pub textures: MaterialTextures,
}

impl LoadedMaterial<'_> {
    /// Borrow this material's scalar PBR fields as a [`Material`].
    pub fn as_material(&self) -> Material<'_> {
        Material {
            name: self.name.as_str(),
            base_colour: self.base_colour,
            metallic: self.metallic,
            roughness: self.roughness,
            emissive: self.emissive,
            emissive_strength: self.emissive_strength,
            clearcoat: self.clearcoat,
            double_sided: self.double_sided,
        }
    }
}

/// A level of detail loaded from a part's `extras.lod`.
#[derive(Debug)]
pub struct LoadedLod<'a> {
    pub mesh: Mesh<'a>,
    pub error: Length,
    pub switch_distance: Length,
}

/// One part reconstructed from the glTF node forest, in the requested [`Up`] frame.
#[derive(Debug)]
pub struct LoadedPart<'a> {
    pub name: String<Alloc<'a>>,
    /// Index of the parent part in [`LoadedGlb::parts`] (always `< self` index), or `None` for a root.
    pub parent: Option<u32>,
    pub transform: Transform,
    pub mesh: Option<Mesh<'a>>,
    pub lods: Vec<LoadedLod<'a>, Alloc<'a>>,
    pub mass: Option<MassFacts>,
}

impl LoadedPart<'_> {
    /// This part's local transform as an affine [`Matrix`].
    pub fn local_matrix(&self) -> Matrix {
        self.transform.to_matrix()
    }
}

/// A decoded `.glb` scene: parts (parent before child), materials, and decoded RGBA8 PNG images.
#[derive(Debug)]
pub struct LoadedGlb<'a> {
    pub parts: Vec<LoadedPart<'a>, Alloc<'a>>,
    pub materials: Vec<LoadedMaterial<'a>, Alloc<'a>>,
    pub images: Vec<LoadedImage<'a>, Alloc<'a>>,
}

impl<'a> LoadedGlb<'a> {
    /// World-space affine matrix of `parts[part_index]`, composed from its root down to it.
    pub fn world_matrix(&self, part_index: usize) -> Matrix {
        let Some(part) = self.parts.get(part_index) else {
            return Matrix::IDENTITY;
        };
        let mut acc = part.local_matrix();
        let mut cur = part.parent;
        let mut steps = 0usize;
        while let Some(p) = cur {
            if steps >= self.parts.len() {
                break;
            }
            steps += 1;
            let Some(parent) = self.parts.get(idx(p)) else {
                break;
            };
            acc = parent.local_matrix().compose(acc);
            cur = parent.parent;
        }
        acc
    }

    /// Look up a decoded embedded image by index.
    pub fn image(&self, index: u32) -> Option<&LoadedImage<'a>> {
        self.images.get(idx(index))
    }

    /// Look up a loaded material by index.
    pub fn material(&self, index: u32) -> Option<&LoadedMaterial<'a>> {
        self.materials.get(idx(index))
    }
}

fn count32(n: usize) -> Result<u32, GlbReadError> {
    len32(n).map_err(|_| GlbReadError::TooLarge { bytes: n.widen_to(), most: u64::from(u32::MAX) })
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn unturn_p3(p: P3, up: Up) -> P3 {
    plus_zeros(match up {
        Up::Y => p,
        Up::Z => [p[0], -p[2], p[1]],
    })
}

// ---------------------------------------------------------------------------------------------
// Bounded, zero-unsafe, `#![no_std]` RFC 8259 JSON parser into a flat scratch arena.

#[derive(Clone, Copy)]
enum JVal {
    Null,
    Bool(bool),
    Uint(u64),
    Float(f64),
    Str(u32, u32),
    Arr(u32, u32),
    Obj(u32, u32),
}

struct JDoc<'s> {
    vals: V<'s, JVal>,
    strings: V<'s, u8>,
    items: V<'s, u32>,
    entries: V<'s, (u32, u32, u32)>,
}

impl<'s> JDoc<'s> {
    fn new(alloc: Alloc<'s>) -> JDoc<'s> {
        JDoc {
            vals: Vec::new_in(alloc),
            strings: Vec::new_in(alloc),
            items: Vec::new_in(alloc),
            entries: Vec::new_in(alloc),
        }
    }

    fn val(&self, id: u32) -> JVal {
        self.vals[idx(id)]
    }

    fn str_span(&self, start: u32, len: u32) -> &str {
        let s = idx(start);
        let l = idx(len);
        // Every byte pushed to `strings` comes from a UTF-8 slice or `char::encode_utf8`.
        core::str::from_utf8(&self.strings[s..s + l]).unwrap_or("")
    }

    fn as_str(&self, id: u32) -> Option<&str> {
        match self.val(id) {
            JVal::Str(s, l) => Some(self.str_span(s, l)),
            _ => None,
        }
    }

    fn as_bool(&self, id: u32) -> Option<bool> {
        match self.val(id) {
            JVal::Bool(b) => Some(b),
            _ => None,
        }
    }

    fn as_u64(&self, id: u32) -> Option<u64> {
        match self.val(id) {
            JVal::Uint(n) => Some(n),
            JVal::Float(f) if (0.0..9_007_199_254_740_992.0).contains(&f) => {
                let u = f as u64;
                if (u as f64) == f { Some(u) } else { None }
            }
            _ => None,
        }
    }

    fn as_f64(&self, id: u32) -> Option<f64> {
        match self.val(id) {
            JVal::Uint(n) => Some(n as f64),
            JVal::Float(f) => Some(f),
            _ => None,
        }
    }

    fn as_arr(&self, id: u32) -> Option<&[u32]> {
        match self.val(id) {
            JVal::Arr(s, e) => Some(&self.items[idx(s)..idx(e)]),
            _ => None,
        }
    }

    fn as_obj(&self, id: u32) -> Option<&[(u32, u32, u32)]> {
        match self.val(id) {
            JVal::Obj(s, e) => Some(&self.entries[idx(s)..idx(e)]),
            _ => None,
        }
    }

    fn get(&self, obj: u32, key: &str) -> Option<u32> {
        let entries = self.as_obj(obj)?;
        let want = key.as_bytes();
        for &(ks, kl, v) in entries {
            let s = idx(ks);
            let l = idx(kl);
            if &self.strings[s..s + l] == want {
                return Some(v);
            }
        }
        None
    }

    fn list(&self, obj: u32, key: &str) -> &[u32] {
        self.get(obj, key).and_then(|id| self.as_arr(id)).unwrap_or(&[])
    }
}

struct JParser<'i, 's> {
    text: &'i str,
    bytes: &'i [u8],
    pos: usize,
    doc: JDoc<'s>,
    scratch: Alloc<'s>,
}

impl<'i, 's> JParser<'i, 's> {
    fn parse(input: &'i [u8], scratch: Alloc<'s>) -> Result<(JDoc<'s>, u32), GlbReadError> {
        let text = core::str::from_utf8(input)
            .map_err(|e| GlbReadError::Json { offset: e.valid_up_to(), why: "invalid UTF-8" })?;
        let mut p = JParser { text, bytes: input, pos: 0, doc: JDoc::new(scratch), scratch };
        p.skip_ws();
        let root = p.parse_value(0)?;
        p.skip_ws();
        if p.pos != p.bytes.len() {
            return Err(GlbReadError::Json { offset: p.pos, why: "trailing tokens after JSON root" });
        }
        Ok((p.doc, root))
    }

    fn err(&self, why: &'static str) -> GlbReadError {
        GlbReadError::Json { offset: self.pos, why }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && matches!(self.bytes[self.pos], b' ' | b'\t' | b'\r' | b'\n') {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn push_val(&mut self, v: JVal) -> Result<u32, GlbReadError> {
        let id = count32(self.doc.vals.len())?;
        self.doc.vals.try_push(v).map_err(|r| r.why)?;
        Ok(id)
    }

    fn parse_value(&mut self, depth: u32) -> Result<u32, GlbReadError> {
        if depth > MAX_JSON_DEPTH {
            return Err(self.err("JSON nesting too deep"));
        }
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.parse_object(depth + 1),
            Some(b'[') => self.parse_array(depth + 1),
            Some(b'"') => {
                let (s, l) = self.parse_string_raw()?;
                self.push_val(JVal::Str(s, l))
            }
            Some(b't') => {
                self.expect_bytes(b"true")?;
                self.push_val(JVal::Bool(true))
            }
            Some(b'f') => {
                self.expect_bytes(b"false")?;
                self.push_val(JVal::Bool(false))
            }
            Some(b'n') => {
                self.expect_bytes(b"null")?;
                self.push_val(JVal::Null)
            }
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            Some(_) => Err(self.err("unexpected character")),
            None => Err(self.err("unexpected end of JSON")),
        }
    }

    fn expect_bytes(&mut self, lit: &[u8]) -> Result<(), GlbReadError> {
        if self.bytes.len() - self.pos < lit.len() || &self.bytes[self.pos..self.pos + lit.len()] != lit {
            return Err(self.err("invalid literal"));
        }
        self.pos += lit.len();
        Ok(())
    }

    fn parse_object(&mut self, depth: u32) -> Result<u32, GlbReadError> {
        self.pos += 1; // '{'
        self.skip_ws();
        let mut local: V<'s, (u32, u32, u32)> = Vec::new_in(self.scratch);
        if self.peek() == Some(b'}') {
            self.pos += 1;
            let s = count32(self.doc.entries.len())?;
            return self.push_val(JVal::Obj(s, s));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.err("expected string key in object"));
            }
            let (ks, kl) = self.parse_string_raw()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(self.err("expected ':' after object key"));
            }
            self.pos += 1;
            let v = self.parse_value(depth)?;
            local.try_push((ks, kl, v)).map_err(|r| r.why)?;
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or '}' in object")),
            }
        }
        let start = count32(self.doc.entries.len())?;
        self.doc.entries.try_extend_from_slice(&local)?;
        let end = count32(self.doc.entries.len())?;
        self.push_val(JVal::Obj(start, end))
    }

    fn parse_array(&mut self, depth: u32) -> Result<u32, GlbReadError> {
        self.pos += 1; // '['
        self.skip_ws();
        let mut local: V<'s, u32> = Vec::new_in(self.scratch);
        if self.peek() == Some(b']') {
            self.pos += 1;
            let s = count32(self.doc.items.len())?;
            return self.push_val(JVal::Arr(s, s));
        }
        loop {
            let v = self.parse_value(depth)?;
            local.try_push(v).map_err(|r| r.why)?;
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or ']' in array")),
            }
        }
        let start = count32(self.doc.items.len())?;
        self.doc.items.try_extend_from_slice(&local)?;
        let end = count32(self.doc.items.len())?;
        self.push_val(JVal::Arr(start, end))
    }

    fn parse_hex4(&mut self) -> Result<u16, GlbReadError> {
        if self.bytes.len() - self.pos < 4 {
            return Err(self.err("truncated \\u escape"));
        }
        let mut acc = 0u16;
        for _ in 0..4 {
            let b = self.bytes[self.pos];
            self.pos += 1;
            let digit = match b {
                b'0'..=b'9' => u16::from(b - b'0'),
                b'a'..=b'f' => u16::from(b - b'a' + 10),
                b'A'..=b'F' => u16::from(b - b'A' + 10),
                _ => return Err(self.err("invalid hex digit in \\u escape")),
            };
            acc = (acc << 4) | digit;
        }
        Ok(acc)
    }

    fn parse_string_raw(&mut self) -> Result<(u32, u32), GlbReadError> {
        self.pos += 1; // '"'
        let start = count32(self.doc.strings.len())?;
        while self.pos < self.bytes.len() {
            let run_start = self.pos;
            while self.pos < self.bytes.len() {
                let b = self.bytes[self.pos];
                if b == b'"' || b == b'\\' || b < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            if self.pos > run_start {
                self.doc.strings.try_extend_from_slice(&self.bytes[run_start..self.pos])?;
            }
            if self.pos >= self.bytes.len() {
                return Err(self.err("unterminated string"));
            }
            match self.bytes[self.pos] {
                b'"' => {
                    self.pos += 1;
                    let end = count32(self.doc.strings.len())?;
                    return Ok((start, end - start));
                }
                b'\\' => {
                    self.pos += 1;
                    if self.pos >= self.bytes.len() {
                        return Err(self.err("unterminated escape"));
                    }
                    let esc = self.bytes[self.pos];
                    self.pos += 1;
                    match esc {
                        b'"' => self.doc.strings.try_push(b'"').map_err(|r| r.why)?,
                        b'\\' => self.doc.strings.try_push(b'\\').map_err(|r| r.why)?,
                        b'/' => self.doc.strings.try_push(b'/').map_err(|r| r.why)?,
                        b'b' => self.doc.strings.try_push(0x08).map_err(|r| r.why)?,
                        b'f' => self.doc.strings.try_push(0x0C).map_err(|r| r.why)?,
                        b'n' => self.doc.strings.try_push(b'\n').map_err(|r| r.why)?,
                        b'r' => self.doc.strings.try_push(b'\r').map_err(|r| r.why)?,
                        b't' => self.doc.strings.try_push(b'\t').map_err(|r| r.why)?,
                        b'u' => {
                            let code = self.parse_hex4()?;
                            let ch = if (0xD800..=0xDBFF).contains(&code) {
                                if self.bytes.len() - self.pos < 2
                                    || self.bytes[self.pos] != b'\\'
                                    || self.bytes[self.pos + 1] != b'u'
                                {
                                    return Err(self.err("unpaired high surrogate"));
                                }
                                self.pos += 2;
                                let low = self.parse_hex4()?;
                                if !(0xDC00..=0xDFFF).contains(&low) {
                                    return Err(self.err("invalid low surrogate"));
                                }
                                let cp = 0x10000u32
                                    + (u32::from(code - 0xD800) << 10)
                                    + u32::from(low - 0xDC00);
                                char::from_u32(cp).ok_or_else(|| self.err("invalid unicode scalar"))?
                            } else if (0xDC00..=0xDFFF).contains(&code) {
                                return Err(self.err("unpaired low surrogate"));
                            } else {
                                char::from_u32(u32::from(code)).ok_or_else(|| self.err("invalid unicode scalar"))?
                            };
                            let mut buf = [0u8; 4];
                            self.doc.strings.try_extend_from_slice(ch.encode_utf8(&mut buf).as_bytes())?;
                        }
                        _ => return Err(self.err("unknown escape sequence")),
                    }
                }
                _ => return Err(self.err("unescaped control character in string")),
            }
        }
        Err(self.err("unterminated string"))
    }

    fn parse_number(&mut self) -> Result<u32, GlbReadError> {
        let start = self.pos;
        let mut has_sign = false;
        let mut is_float = false;
        if self.peek() == Some(b'-') {
            has_sign = true;
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                if self.peek().is_some_and(|b| b.is_ascii_digit()) {
                    return Err(self.err("leading zero in number"));
                }
            }
            Some(b'1'..=b'9') => {
                while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.err("invalid number")),
        }
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            if !self.peek().is_some_and(|b| b.is_ascii_digit()) {
                return Err(self.err("expected digit after '.'"));
            }
            while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !self.peek().is_some_and(|b| b.is_ascii_digit()) {
                return Err(self.err("expected digit in exponent"));
            }
            while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        let slice = &self.text[start..self.pos];
        if !has_sign
            && !is_float
            && let Ok(n) = slice.parse::<u64>()
        {
            return self.push_val(JVal::Uint(n));
        }
        let f = slice.parse::<f64>().map_err(|_| self.err("unparseable float"))?;
        if !f.is_finite() {
            return Err(self.err("non-finite number"));
        }
        self.push_val(JVal::Float(f))
    }
}

// ---------------------------------------------------------------------------------------------
// glTF bufferViews, accessors, and meshes.

#[derive(Clone, Copy)]
struct RawView {
    offset: usize,
    length: usize,
    stride: Option<usize>,
}

#[derive(Clone, Copy)]
struct RawAcc {
    view: usize,
    offset: usize,
    component: u64,
    normalized: bool,
    count: u32,
    comps: u32,
    comp_size: usize,
    stride: usize,
}

fn component_size(component: u64) -> Option<usize> {
    match component {
        BYTE | UNSIGNED_BYTE => Some(1),
        SHORT | UNSIGNED_SHORT => Some(2),
        UNSIGNED_INT | FLOAT => Some(4),
        _ => None,
    }
}

fn type_comps(kind: &str) -> Option<u32> {
    match kind {
        "SCALAR" => Some(1),
        "VEC2" => Some(2),
        "VEC3" => Some(3),
        "VEC4" => Some(4),
        _ => None,
    }
}

fn read_component_raw(bin: &[u8], at: usize, component: u64) -> f64 {
    match component {
        BYTE => f64::from(i8::from_le_bytes([bin[at]])),
        UNSIGNED_BYTE => f64::from(bin[at]),
        SHORT => f64::from(i16::from_le_bytes([bin[at], bin[at + 1]])),
        UNSIGNED_SHORT => f64::from(u16::from_le_bytes([bin[at], bin[at + 1]])),
        UNSIGNED_INT => f64::from(u32::from_le_bytes([bin[at], bin[at + 1], bin[at + 2], bin[at + 3]])),
        _ => f64::from(f32::from_le_bytes([bin[at], bin[at + 1], bin[at + 2], bin[at + 3]])),
    }
}

fn read_component_norm(bin: &[u8], at: usize, acc: RawAcc) -> f64 {
    let raw = read_component_raw(bin, at, acc.component);
    if !acc.normalized {
        return raw;
    }
    match acc.component {
        BYTE => (raw / 127.0).max(-1.0),
        UNSIGNED_BYTE => raw / 255.0,
        SHORT => (raw / 32767.0).max(-1.0),
        UNSIGNED_SHORT => raw / 65535.0,
        _ => raw,
    }
}

fn read_index_u32(bin: &[u8], at: usize, component: u64) -> u32 {
    match component {
        UNSIGNED_BYTE => u32::from(bin[at]),
        UNSIGNED_SHORT => u32::from(u16::from_le_bytes([bin[at], bin[at + 1]])),
        _ => u32::from_le_bytes([bin[at], bin[at + 1], bin[at + 2], bin[at + 3]]),
    }
}

fn read_f32_le(bin: &[u8], at: usize) -> f32 {
    f32::from_le_bytes([bin[at], bin[at + 1], bin[at + 2], bin[at + 3]])
}

fn ref_index(index: u64, len: usize, what: &'static str) -> Result<usize, GlbReadError> {
    let Ok(i) = index.narrow::<usize>() else {
        return Err(GlbReadError::OutOfRange { what, index, len });
    };
    if i < len { Ok(i) } else { Err(GlbReadError::OutOfRange { what, index, len }) }
}

fn fixed_f64<const N: usize>(doc: &JDoc<'_>, obj: u32, key: &str, why: &'static str) -> Result<Option<[f64; N]>, GlbReadError> {
    let Some(id) = doc.get(obj, key) else {
        return Ok(None);
    };
    let Some(arr) = doc.as_arr(id) else {
        return Err(GlbReadError::InvalidStructure { why });
    };
    if arr.len() != N {
        return Err(GlbReadError::InvalidStructure { why });
    }
    let mut out = [0.0; N];
    for (dst, &item) in out.iter_mut().zip(arr) {
        let Some(v) = doc.as_f64(item) else {
            return Err(GlbReadError::InvalidStructure { why });
        };
        *dst = v;
    }
    Ok(Some(out))
}

#[derive(Clone, Copy)]
struct RawPrim {
    position: usize,
    normal: Option<usize>,
    texcoord: Option<usize>,
    tangent: Option<usize>,
    indices: Option<usize>,
    material: Option<u32>,
}

struct ReadCtx<'b, 's> {
    bin: &'b [u8],
    views: &'b [RawView],
    accs: &'b [RawAcc],
    meshes: &'b [(u32, u32)],
    prims: &'b [RawPrim],
    up: Up,
    scratch: Alloc<'s>,
}

impl ReadCtx<'_, '_> {
    fn acc_offset(&self, acc: RawAcc, elem: u32, comp: u32) -> usize {
        self.views[acc.view].offset + acc.offset + idx(elem) * acc.stride + idx(comp) * acc.comp_size
    }

    fn read_positions(
        &self,
        acc_idx: usize,
        dequant: Option<([f64; 3], [f64; 3])>,
        out: &mut V<'_, P3>,
    ) -> Result<(), GlbReadError> {
        let acc = self.accs[acc_idx];
        for v in 0..acc.count {
            let p_gltf = if acc.component == FLOAT {
                [
                    read_f32_le(self.bin, self.acc_offset(acc, v, 0)),
                    read_f32_le(self.bin, self.acc_offset(acc, v, 1)),
                    read_f32_le(self.bin, self.acc_offset(acc, v, 2)),
                ]
            } else {
                let (offset, scale) = dequant.unwrap_or(([0.0; 3], [1.0; 3]));
                let mut p = [0.0f32; 3];
                for a in 0..3 {
                    let q = read_component_norm(self.bin, self.acc_offset(acc, v, a), acc);
                    let ai = idx(a);
                    p[ai] = q as f32 * scale[ai] as f32 + offset[ai] as f32;
                }
                p
            };
            out.try_push(unturn_p3(p_gltf, self.up)).map_err(|r| r.why)?;
        }
        Ok(())
    }

    fn read_normals(
        &self,
        acc_idx: usize,
        quantized_pos: bool,
        dequant: Option<([f64; 3], [f64; 3])>,
        mesh_id: u32,
        out: &mut V<'_, P3>,
    ) -> Result<(), GlbReadError> {
        let acc = self.accs[acc_idx];
        let inv_scale = if quantized_pos {
            dequant.map(|(_, s)| s)
        } else {
            None
        };
        for v in 0..acc.count {
            let nx = read_component_norm(self.bin, self.acc_offset(acc, v, 0), acc);
            let ny = read_component_norm(self.bin, self.acc_offset(acc, v, 1), acc);
            let nz = read_component_norm(self.bin, self.acc_offset(acc, v, 2), acc);
            let n_gltf = match inv_scale {
                Some(scale) => {
                    if scale[0] == 0.0 || scale[1] == 0.0 || scale[2] == 0.0 {
                        return Err(GlbReadError::BadMesh { mesh: mesh_id, why: "zero dequantize scale" });
                    }
                    let Some(u) = unit([nx / scale[0], ny / scale[1], nz / scale[2]]) else {
                        return Err(GlbReadError::BadMesh { mesh: mesh_id, why: "zero quantized normal" });
                    };
                    narrow(u)
                }
                None => [nx as f32, ny as f32, nz as f32],
            };
            out.try_push(unturn_p3(n_gltf, self.up)).map_err(|r| r.why)?;
        }
        Ok(())
    }

    fn read_uvs(&self, acc_idx: usize, out: &mut V<'_, [f32; 2]>) -> Result<(), GlbReadError> {
        let acc = self.accs[acc_idx];
        for v in 0..acc.count {
            let u = read_component_norm(self.bin, self.acc_offset(acc, v, 0), acc) as f32;
            let w = read_component_norm(self.bin, self.acc_offset(acc, v, 1), acc) as f32;
            out.try_push([plus_zero(u), plus_zero(w)]).map_err(|r| r.why)?;
        }
        Ok(())
    }

    fn read_tangents(&self, acc_idx: usize, out: &mut V<'_, [f32; 4]>) -> Result<(), GlbReadError> {
        let acc = self.accs[acc_idx];
        for v in 0..acc.count {
            let tx = read_component_norm(self.bin, self.acc_offset(acc, v, 0), acc) as f32;
            let ty = read_component_norm(self.bin, self.acc_offset(acc, v, 1), acc) as f32;
            let tz = read_component_norm(self.bin, self.acc_offset(acc, v, 2), acc) as f32;
            let tw = read_component_norm(self.bin, self.acc_offset(acc, v, 3), acc) as f32;
            let t3 = unturn_p3([tx, ty, tz], self.up);
            out.try_push([t3[0], t3[1], t3[2], tw]).map_err(|r| r.why)?;
        }
        Ok(())
    }

    fn read_prim_triangles(
        &self,
        prim: RawPrim,
        base_vertex: u32,
        local_vertices: u32,
        part_id: PartId,
        mesh_id: u32,
        triangles: &mut V<'_, [u32; 3]>,
        parts: &mut V<'_, PartId>,
        roles: &mut V<'_, MaterialRole>,
    ) -> Result<(), GlbReadError> {
        let role = MaterialRole(prim.material.unwrap_or(0));
        match prim.indices {
            Some(idx_acc_id) => {
                let acc = self.accs[idx_acc_id];
                if acc.count % 3 != 0 {
                    return Err(GlbReadError::BadMesh { mesh: mesh_id, why: "index count not a multiple of 3" });
                }
                let tri_count = acc.count / 3;
                for t in 0..tri_count {
                    let mut tri = [0u32; 3];
                    for c in 0..3u32 {
                        let off = self.acc_offset(acc, 3 * t + c, 0);
                        let v = read_index_u32(self.bin, off, acc.component);
                        if v >= local_vertices {
                            return Err(GlbReadError::BadMesh { mesh: mesh_id, why: "triangle index past vertex count" });
                        }
                        tri[idx(c)] = base_vertex.checked_add(v).ok_or(GlbReadError::BadMesh {
                            mesh: mesh_id,
                            why: "vertex index overflow",
                        })?;
                    }
                    triangles.try_push(tri).map_err(|r| r.why)?;
                    parts.try_push(part_id).map_err(|r| r.why)?;
                    roles.try_push(role).map_err(|r| r.why)?;
                }
            }
            None => {
                if local_vertices % 3 != 0 {
                    return Err(GlbReadError::BadMesh {
                        mesh: mesh_id,
                        why: "non-indexed vertex count not a multiple of 3",
                    });
                }
                let tri_count = local_vertices / 3;
                for t in 0..tri_count {
                    let i0 = base_vertex + 3 * t;
                    triangles.try_push([i0, i0 + 1, i0 + 2]).map_err(|r| r.why)?;
                    parts.try_push(part_id).map_err(|r| r.why)?;
                    roles.try_push(role).map_err(|r| r.why)?;
                }
            }
        }
        Ok(())
    }

    fn build_mesh<'a>(
        &self,
        mesh_idx: usize,
        part_id: PartId,
        dequant: Option<([f64; 3], [f64; 3])>,
        alloc: Alloc<'a>,
    ) -> Result<Mesh<'a>, GlbReadError> {
        let mesh_id = count32(mesh_idx)?;
        let (ps, pe) = self.meshes[mesh_idx];
        let prims = &self.prims[idx(ps)..idx(pe)];
        if prims.is_empty() {
            return Err(GlbReadError::BadMesh { mesh: mesh_id, why: "mesh has no primitives" });
        }

        let first = prims[0];
        let shared = prims.iter().all(|p| {
            p.position == first.position
                && p.normal == first.normal
                && p.texcoord == first.texcoord
                && p.tangent == first.tangent
        });

        let mut positions: V<'a, P3> = Vec::new_in(alloc);
        let mut normals: V<'a, P3> = Vec::new_in(alloc);
        let mut uvs: V<'a, [f32; 2]> = Vec::new_in(alloc);
        let mut tangents: V<'a, [f32; 4]> = Vec::new_in(alloc);
        let mut triangles: V<'a, [u32; 3]> = Vec::new_in(alloc);
        let mut parts: V<'a, PartId> = Vec::new_in(alloc);
        let mut roles: V<'a, MaterialRole> = Vec::new_in(alloc);

        if shared {
            let pos_acc = self.accs[first.position];
            let nv = pos_acc.count;
            let quantized_pos = pos_acc.component != FLOAT;
            positions.try_reserve(idx(nv))?;
            self.read_positions(first.position, dequant, &mut positions)?;
            if let Some(norm_id) = first.normal {
                normals.try_reserve(idx(nv))?;
                self.read_normals(norm_id, quantized_pos, dequant, mesh_id, &mut normals)?;
            }
            if let Some(uv_id) = first.texcoord {
                uvs.try_reserve(idx(nv))?;
                self.read_uvs(uv_id, &mut uvs)?;
            }
            if let Some(tan_id) = first.tangent {
                tangents.try_reserve(idx(nv))?;
                self.read_tangents(tan_id, &mut tangents)?;
            }
            for &prim in prims {
                self.read_prim_triangles(prim, 0, nv, part_id, mesh_id, &mut triangles, &mut parts, &mut roles)?;
            }
            if first.normal.is_none() {
                synthesize_normals(&positions, &triangles, self.scratch, &mut normals)?;
            }
        } else {
            let all_uv = prims.iter().all(|p| p.texcoord.is_some());
            let all_tan = prims.iter().all(|p| p.tangent.is_some());
            let all_norm = prims.iter().all(|p| p.normal.is_some());
            for &prim in prims {
                let base_vertex = count32(positions.len())?;
                let pos_acc = self.accs[prim.position];
                let nv = pos_acc.count;
                let quantized_pos = pos_acc.component != FLOAT;
                self.read_positions(prim.position, dequant, &mut positions)?;
                if all_norm
                    && let Some(norm_id) = prim.normal
                {
                    self.read_normals(norm_id, quantized_pos, dequant, mesh_id, &mut normals)?;
                }
                if all_uv
                    && let Some(uv_id) = prim.texcoord
                {
                    self.read_uvs(uv_id, &mut uvs)?;
                }
                if all_tan
                    && let Some(tan_id) = prim.tangent
                {
                    self.read_tangents(tan_id, &mut tangents)?;
                }
                self.read_prim_triangles(prim, base_vertex, nv, part_id, mesh_id, &mut triangles, &mut parts, &mut roles)?;
            }
            if !all_norm {
                normals.clear();
                synthesize_normals(&positions, &triangles, self.scratch, &mut normals)?;
            }
        }

        let vecs = MeshVecs {
            positions,
            normals,
            tangents,
            uvs,
            colours: Vec::new_in(alloc),
            triangles,
            parts,
            roles,
            creases: Vec::new_in(alloc),
        };
        Mesh::new(vecs).map_err(GlbReadError::Shape)
    }
}

/// Area-weighted vertex normals for external glTF primitives that omit `NORMAL` (glTF §3.7.2.1).
fn synthesize_normals(
    positions: &[P3],
    triangles: &[[u32; 3]],
    scratch: Alloc<'_>,
    out: &mut V<'_, P3>,
) -> Result<(), GlbReadError> {
    let mut accum: V<'_, [f64; 3]> = Vec::try_with_capacity_in(positions.len(), scratch)?;
    for _ in 0..positions.len() {
        accum.try_push([0.0; 3]).map_err(|r| r.why)?;
    }
    for &[i0, i1, i2] in triangles {
        let a = wide(positions[idx(i0)]);
        let b = wide(positions[idx(i1)]);
        let c = wide(positions[idx(i2)]);
        let n = cross(sub(b, a), sub(c, a));
        for v in [i0, i1, i2] {
            let vi = idx(v);
            accum[vi] = add(accum[vi], n);
        }
    }
    out.try_reserve(positions.len())?;
    for &n in accum.iter() {
        let dir = match unit(n) {
            Some(u) => plus_zeros(narrow(u)),
            None => [0.0, 0.0, 1.0],
        };
        out.try_push(dir).map_err(|r| r.why)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Raw node metadata extracted from glTF JSON.

#[derive(Clone, Copy)]
struct RawLod {
    mesh: usize,
    error_m: f64,
    switch_m: f64,
}

#[derive(Clone, Copy)]
struct RawNode<'j> {
    name: &'j str,
    children: (u32, u32),
    mesh: Option<usize>,
    has_matrix: bool,
    matrix: Matrix,
    translation: [f64; 3],
    rotation: [f64; 4],
    scale: [f64; 3],
    has_rotation: bool,
    has_extras: bool,
    dequantize: Option<([f64; 3], [f64; 3])>,
    mass: Option<MassFacts>,
    lods: (u32, u32),
}

/// Read a binary glTF 2.0 (`.glb`) into owned parts, meshes, materials, and decoded PNG images.
pub fn read_glb<'a>(bytes: &[u8], opts: GlbReadOpts<'a, '_>) -> Result<LoadedGlb<'a>, GlbReadError> {
    let s = opts.scratch;
    let a = opts.alloc;

    // 1. Container header & chunks.
    let most = u64::from(opts.limits.most_glb_bytes);
    if bytes.len().widen_to::<u64>() > most {
        return Err(GlbReadError::TooLarge { bytes: bytes.len().widen_to(), most });
    }
    if bytes.len() < 20 {
        return Err(GlbReadError::TooShort { len: bytes.len() });
    }
    if &bytes[0..4] != b"glTF" {
        return Err(GlbReadError::BadMagic);
    }
    let version = le32(bytes, 4);
    if version != 2 {
        return Err(GlbReadError::UnsupportedVersion { version });
    }
    let declared = le32(bytes, 8);
    if idx(declared) != bytes.len() {
        return Err(GlbReadError::LengthMismatch { declared, actual: bytes.len() });
    }
    let json_len = idx(le32(bytes, 12));
    if &bytes[16..20] != b"JSON" {
        return Err(GlbReadError::BadChunk { why: "first chunk is not JSON" });
    }
    if json_len == 0 || json_len % 4 != 0 || 20 + json_len > bytes.len() {
        return Err(GlbReadError::BadChunk { why: "invalid JSON chunk length" });
    }
    let json_bytes = &bytes[20..20 + json_len];
    let mut at = 20 + json_len;
    let mut bin: &[u8] = &[];
    let mut seen_bin = false;
    while at < bytes.len() {
        if bytes.len() - at < 8 {
            return Err(GlbReadError::BadChunk { why: "truncated chunk header" });
        }
        let chunk_len = idx(le32(bytes, at));
        let chunk_type = &bytes[at + 4..at + 8];
        if chunk_len % 4 != 0 || bytes.len() - at - 8 < chunk_len {
            return Err(GlbReadError::BadChunk { why: "invalid chunk length" });
        }
        let payload = &bytes[at + 8..at + 8 + chunk_len];
        if chunk_type == b"BIN\0" {
            if seen_bin {
                return Err(GlbReadError::BadChunk { why: "duplicate BIN chunk" });
            }
            seen_bin = true;
            bin = payload;
        }
        at += 8 + chunk_len;
    }

    // 2. Parse JSON root and validate glTF 2.0 header.
    let (doc, root) = JParser::parse(json_bytes, s)?;
    if doc.as_obj(root).is_none() {
        return Err(GlbReadError::InvalidStructure { why: "JSON root is not an object" });
    }
    let asset = doc.get(root, "asset").ok_or(GlbReadError::InvalidStructure { why: "missing asset" })?;
    let ver_str = doc
        .get(asset, "version")
        .and_then(|v| doc.as_str(v))
        .ok_or(GlbReadError::InvalidStructure { why: "missing asset.version" })?;
    if ver_str != "2.0" {
        return Err(GlbReadError::InvalidStructure { why: "asset.version is not 2.0" });
    }

    // 3. Buffers and bufferViews.
    let buffers = doc.list(root, "buffers");
    let buffer_len = match buffers.len() {
        0 => 0usize,
        1 => {
            let bl = doc
                .get(buffers[0], "byteLength")
                .and_then(|v| doc.as_u64(v))
                .ok_or(GlbReadError::InvalidStructure { why: "buffers[0].byteLength missing" })?;
            let bl_usize = bl.narrow::<usize>().map_err(|_| GlbReadError::InvalidStructure {
                why: "buffers[0].byteLength out of range",
            })?;
            if bl_usize > bin.len() {
                return Err(GlbReadError::InvalidStructure { why: "buffers[0].byteLength exceeds BIN chunk" });
            }
            bl_usize
        }
        _ => return Err(GlbReadError::InvalidStructure { why: "GLB supports at most one buffer" }),
    };

    let raw_views_json = doc.list(root, "bufferViews");
    let mut views: V<'_, RawView> = Vec::try_with_capacity_in(raw_views_json.len(), s)?;
    for &vj in raw_views_json {
        let buf = doc
            .get(vj, "buffer")
            .and_then(|v| doc.as_u64(v))
            .ok_or(GlbReadError::InvalidStructure { why: "bufferView.buffer missing" })?;
        ref_index(buf, buffers.len(), "bufferView.buffer")?;
        let offset = match doc.get(vj, "byteOffset") {
            Some(v) => doc
                .as_u64(v)
                .and_then(|x| x.narrow::<usize>().ok())
                .ok_or(GlbReadError::InvalidStructure { why: "bad bufferView.byteOffset" })?,
            None => 0,
        };
        let length = doc
            .get(vj, "byteLength")
            .and_then(|v| doc.as_u64(v))
            .and_then(|x| x.narrow::<usize>().ok())
            .ok_or(GlbReadError::InvalidStructure { why: "bad bufferView.byteLength" })?;
        let end = offset
            .checked_add(length)
            .ok_or(GlbReadError::InvalidStructure { why: "bufferView range overflow" })?;
        if length == 0 || end > buffer_len {
            return Err(GlbReadError::InvalidStructure { why: "bufferView past buffer" });
        }
        let stride = match doc.get(vj, "byteStride") {
            None => None,
            Some(v) => {
                let st = doc
                    .as_u64(v)
                    .and_then(|x| x.narrow::<usize>().ok())
                    .ok_or(GlbReadError::InvalidStructure { why: "bad bufferView.byteStride" })?;
                if !(4..=252).contains(&st) || st % 4 != 0 {
                    return Err(GlbReadError::InvalidStructure { why: "bufferView.byteStride out of range" });
                }
                Some(st)
            }
        };
        views.try_push(RawView { offset, length, stride }).map_err(|r| r.why)?;
    }

    // 4. Accessors.
    let raw_accs_json = doc.list(root, "accessors");
    let mut accs: V<'_, RawAcc> = Vec::try_with_capacity_in(raw_accs_json.len(), s)?;
    for (ai, &aj) in raw_accs_json.iter().enumerate() {
        let acc_id = count32(ai)?;
        let bad = |why| GlbReadError::BadAccessor { accessor: acc_id, why };
        if doc.get(aj, "sparse").is_some() {
            return Err(bad("sparse accessors are not supported"));
        }
        let view_u = doc.get(aj, "bufferView").and_then(|v| doc.as_u64(v)).ok_or(bad("missing bufferView"))?;
        let view = ref_index(view_u, views.len(), "accessor.bufferView")?;
        let component = doc.get(aj, "componentType").and_then(|v| doc.as_u64(v)).ok_or(bad("missing componentType"))?;
        let comp_size = component_size(component).ok_or(bad("unknown componentType"))?;
        let comps = doc
            .get(aj, "type")
            .and_then(|v| doc.as_str(v))
            .and_then(type_comps)
            .ok_or(bad("unknown accessor type"))?;
        let count_u = doc.get(aj, "count").and_then(|v| doc.as_u64(v)).ok_or(bad("missing count"))?;
        let count = count_u.narrow::<u32>().map_err(|_| bad("count exceeds u32"))?;
        if count == 0 {
            return Err(bad("zero count"));
        }
        let offset = match doc.get(aj, "byteOffset") {
            Some(v) => doc.as_u64(v).and_then(|x| x.narrow::<usize>().ok()).ok_or(bad("bad byteOffset"))?,
            None => 0,
        };
        let normalized = match doc.get(aj, "normalized") {
            Some(v) => doc.as_bool(v).ok_or(bad("normalized not a bool"))?,
            None => false,
        };
        let vrec = views[view];
        let elem_bytes = idx(comps) * comp_size;
        let stride = vrec.stride.unwrap_or(elem_bytes);
        if stride < elem_bytes {
            return Err(bad("stride smaller than element size"));
        }
        let need = idx(count - 1)
            .checked_mul(stride)
            .and_then(|n| n.checked_add(elem_bytes))
            .and_then(|n| n.checked_add(offset))
            .ok_or(bad("byte range overflow"))?;
        if need > vrec.length {
            return Err(bad("accessor exceeds bufferView"));
        }
        accs.try_push(RawAcc { view, offset, component, normalized, count, comps, comp_size, stride })
            .map_err(|r| r.why)?;
    }

    // 5. Embedded PNG images, textures, and materials.
    let images_json = doc.list(root, "images");
    let mut images: V<'a, LoadedImage<'a>> = Vec::try_with_capacity_in(images_json.len(), a)?;
    for (ii, &ij) in images_json.iter().enumerate() {
        let img_id = count32(ii)?;
        if let Some(mime) = doc.get(ij, "mimeType") {
            let mime_str = doc.as_str(mime).ok_or(GlbReadError::InvalidStructure { why: "image.mimeType not a string" })?;
            if mime_str != "image/png" {
                return Err(GlbReadError::InvalidStructure { why: "only image/png embedded images are supported" });
            }
        }
        let bv_u = doc
            .get(ij, "bufferView")
            .and_then(|v| doc.as_u64(v))
            .ok_or(GlbReadError::InvalidStructure { why: "image missing bufferView" })?;
        let bv = ref_index(bv_u, views.len(), "image.bufferView")?;
        let vrec = views[bv];
        let img_bytes = &bin[vrec.offset..vrec.offset + vrec.length];
        let decoded = decode_png(img_bytes, opts.limits, a, s).map_err(|error| GlbReadError::Png { image: img_id, error })?;
        let rgba = match decoded.kind {
            PngKind::Rgba8 => decoded.pixels,
            PngKind::Grey8 => {
                let mut expanded: V<'a, u8> = Vec::try_with_capacity_in(decoded.pixels.len() * 4, a)?;
                for &g in decoded.pixels.iter() {
                    expanded.try_extend_from_slice(&[g, g, g, 255])?;
                }
                expanded
            }
        };
        images.try_push(LoadedImage { width: decoded.width, height: decoded.height, rgba }).map_err(|r| r.why)?;
    }

    let textures_json = doc.list(root, "textures");
    let mut tex_to_image: V<'_, u32> = Vec::try_with_capacity_in(textures_json.len(), s)?;
    for &tj in textures_json {
        let src_u = doc
            .get(tj, "source")
            .and_then(|v| doc.as_u64(v))
            .ok_or(GlbReadError::InvalidStructure { why: "texture missing source" })?;
        let src = ref_index(src_u, images.len(), "texture.source")?;
        tex_to_image.try_push(count32(src)?).map_err(|r| r.why)?;
    }

    let resolve_tex = |obj: u32, key: &str| -> Result<Option<u32>, GlbReadError> {
        let Some(info) = doc.get(obj, key) else {
            return Ok(None);
        };
        let ti = doc
            .get(info, "index")
            .and_then(|v| doc.as_u64(v))
            .ok_or(GlbReadError::InvalidStructure { why: "textureInfo missing index" })?;
        let idx_t = ref_index(ti, tex_to_image.len(), "textureInfo.index")?;
        Ok(Some(tex_to_image[idx_t]))
    };

    let materials_json = doc.list(root, "materials");
    let mut materials: V<'a, LoadedMaterial<'a>> = Vec::try_with_capacity_in(materials_json.len(), a)?;
    for &mj in materials_json {
        let name_str = doc.get(mj, "name").and_then(|v| doc.as_str(v)).unwrap_or("");
        let name = String::try_from_str_in(name_str, a)?;
        let mut base_colour = [1.0; 4];
        let mut metallic = 1.0;
        let mut roughness = 1.0;
        let mut textures = MaterialTextures::NONE;
        if let Some(pbr) = doc.get(mj, "pbrMetallicRoughness") {
            if let Some(bc) = fixed_f64::<4>(&doc, pbr, "baseColorFactor", "bad baseColorFactor")? {
                base_colour = bc;
            }
            if let Some(m) = doc.get(pbr, "metallicFactor") {
                metallic = doc.as_f64(m).ok_or(GlbReadError::InvalidStructure { why: "bad metallicFactor" })?;
            }
            if let Some(r) = doc.get(pbr, "roughnessFactor") {
                roughness = doc.as_f64(r).ok_or(GlbReadError::InvalidStructure { why: "bad roughnessFactor" })?;
            }
            textures.base_colour = resolve_tex(pbr, "baseColorTexture")?;
            textures.metallic_roughness = resolve_tex(pbr, "metallicRoughnessTexture")?;
        }
        if let Some(nt) = doc.get(mj, "normalTexture") {
            let ti = doc
                .get(nt, "index")
                .and_then(|v| doc.as_u64(v))
                .ok_or(GlbReadError::InvalidStructure { why: "normalTexture missing index" })?;
            let idx_t = ref_index(ti, tex_to_image.len(), "normalTexture.index")?;
            textures.normal = Some(tex_to_image[idx_t]);
            if let Some(sc) = doc.get(nt, "scale") {
                textures.normal_scale =
                    doc.as_f64(sc).ok_or(GlbReadError::InvalidStructure { why: "bad normalTexture.scale" })?;
            }
        }
        let emissive = fixed_f64::<3>(&doc, mj, "emissiveFactor", "bad emissiveFactor")?.unwrap_or([0.0; 3]);
        let double_sided = match doc.get(mj, "doubleSided") {
            Some(v) => doc.as_bool(v).ok_or(GlbReadError::InvalidStructure { why: "bad doubleSided" })?,
            None => false,
        };
        let mut clearcoat = None;
        let mut emissive_strength = None;
        if let Some(ext) = doc.get(mj, "extensions") {
            if let Some(cc) = doc.get(ext, "KHR_materials_clearcoat") {
                let factor = match doc.get(cc, "clearcoatFactor") {
                    Some(v) => doc.as_f64(v).ok_or(GlbReadError::InvalidStructure { why: "bad clearcoatFactor" })?,
                    None => 0.0,
                };
                let cc_rough = match doc.get(cc, "clearcoatRoughnessFactor") {
                    Some(v) => doc
                        .as_f64(v)
                        .ok_or(GlbReadError::InvalidStructure { why: "bad clearcoatRoughnessFactor" })?,
                    None => 0.0,
                };
                clearcoat = Some(Clearcoat { factor, roughness: cc_rough });
            }
            if let Some(es) = doc.get(ext, "KHR_materials_emissive_strength") {
                let strength = match doc.get(es, "emissiveStrength") {
                    Some(v) => doc.as_f64(v).ok_or(GlbReadError::InvalidStructure { why: "bad emissiveStrength" })?,
                    None => 1.0,
                };
                emissive_strength = Some(strength);
            }
        }
        materials
            .try_push(LoadedMaterial {
                name,
                base_colour,
                metallic,
                roughness,
                emissive,
                emissive_strength,
                clearcoat,
                double_sided,
                textures,
            })
            .map_err(|r| r.why)?;
    }

    // 6. Meshes and primitives metadata.
    let meshes_json = doc.list(root, "meshes");
    let mut meshes_meta: V<'_, (u32, u32)> = Vec::try_with_capacity_in(meshes_json.len(), s)?;
    let mut prims_meta: V<'_, RawPrim> = Vec::new_in(s);
    for (mi, &mj) in meshes_json.iter().enumerate() {
        let mesh_id = count32(mi)?;
        let bad = |why| GlbReadError::BadMesh { mesh: mesh_id, why };
        let prims_json = doc.list(mj, "primitives");
        if prims_json.is_empty() {
            return Err(bad("mesh has no primitives"));
        }
        let p_start = count32(prims_meta.len())?;
        for &pj in prims_json {
            if let Some(mode) = doc.get(pj, "mode") {
                let m = doc.as_u64(mode).ok_or(bad("bad primitive mode"))?;
                if m != 4 {
                    return Err(bad("only TRIANGLES (mode 4) primitives are supported"));
                }
            }
            let attrs = doc.get(pj, "attributes").ok_or(bad("primitive missing attributes"))?;
            let pos_u = doc.get(attrs, "POSITION").and_then(|v| doc.as_u64(v)).ok_or(bad("primitive missing POSITION"))?;
            let position = ref_index(pos_u, accs.len(), "attributes.POSITION")?;
            if accs[position].comps != 3 {
                return Err(bad("POSITION accessor is not VEC3"));
            }
            let nv = accs[position].count;

            let normal = match doc.get(attrs, "NORMAL") {
                Some(v) => {
                    let ni = ref_index(doc.as_u64(v).ok_or(bad("bad NORMAL"))?, accs.len(), "attributes.NORMAL")?;
                    if accs[ni].comps != 3 || accs[ni].count != nv {
                        return Err(bad("NORMAL accessor mismatch"));
                    }
                    Some(ni)
                }
                None => None,
            };
            let texcoord = match doc.get(attrs, "TEXCOORD_0") {
                Some(v) => {
                    let ti = ref_index(doc.as_u64(v).ok_or(bad("bad TEXCOORD_0"))?, accs.len(), "attributes.TEXCOORD_0")?;
                    if accs[ti].comps != 2 || accs[ti].count != nv {
                        return Err(bad("TEXCOORD_0 accessor mismatch"));
                    }
                    Some(ti)
                }
                None => None,
            };
            let tangent = match doc.get(attrs, "TANGENT") {
                Some(v) => {
                    let ti = ref_index(doc.as_u64(v).ok_or(bad("bad TANGENT"))?, accs.len(), "attributes.TANGENT")?;
                    if accs[ti].comps != 4 || accs[ti].count != nv {
                        return Err(bad("TANGENT accessor mismatch"));
                    }
                    Some(ti)
                }
                None => None,
            };
            let indices = match doc.get(pj, "indices") {
                Some(v) => {
                    let ii = ref_index(doc.as_u64(v).ok_or(bad("bad indices"))?, accs.len(), "primitive.indices")?;
                    let ia = accs[ii];
                    if ia.comps != 1
                        || ia.normalized
                        || !matches!(ia.component, UNSIGNED_BYTE | UNSIGNED_SHORT | UNSIGNED_INT)
                    {
                        return Err(bad("indices accessor is not unsigned SCALAR"));
                    }
                    Some(ii)
                }
                None => None,
            };
            let material = match doc.get(pj, "material") {
                Some(v) => {
                    let mu = doc.as_u64(v).ok_or(bad("bad material"))?;
                    let m_idx = ref_index(mu, materials.len(), "primitive.material")?;
                    Some(count32(m_idx)?)
                }
                None => None,
            };
            prims_meta
                .try_push(RawPrim { position, normal, texcoord, tangent, indices, material })
                .map_err(|r| r.why)?;
        }
        let p_end = count32(prims_meta.len())?;
        meshes_meta.try_push((p_start, p_end)).map_err(|r| r.why)?;
    }

    // 7. Nodes metadata.
    let nodes_json = doc.list(root, "nodes");
    let mut raw_nodes: V<'_, RawNode<'_>> = Vec::try_with_capacity_in(nodes_json.len(), s)?;
    let mut node_children: V<'_, u32> = Vec::new_in(s);
    let mut raw_lods: V<'_, RawLod> = Vec::new_in(s);

    for &nj in nodes_json {
        let name = doc.get(nj, "name").and_then(|v| doc.as_str(v)).unwrap_or("");
        let c_start = count32(node_children.len())?;
        for &cj in doc.list(nj, "children") {
            let cu = doc.as_u64(cj).ok_or(GlbReadError::InvalidStructure { why: "node child not an integer" })?;
            let ci = ref_index(cu, nodes_json.len(), "node.children")?;
            node_children.try_push(count32(ci)?).map_err(|r| r.why)?;
        }
        let c_end = count32(node_children.len())?;

        let mesh = match doc.get(nj, "mesh") {
            Some(v) => {
                let mu = doc.as_u64(v).ok_or(GlbReadError::InvalidStructure { why: "node.mesh not an integer" })?;
                Some(ref_index(mu, meshes_meta.len(), "node.mesh")?)
            }
            None => None,
        };

        let mut has_matrix = false;
        let mut matrix = Matrix::IDENTITY;
        if let Some(m) = fixed_f64::<16>(&doc, nj, "matrix", "bad node.matrix")? {
            has_matrix = true;
            matrix = Matrix {
                linear: [[m[0], m[4], m[8]], [m[1], m[5], m[9]], [m[2], m[6], m[10]]],
                translation: [m[12], m[13], m[14]],
            };
        }
        let translation = fixed_f64::<3>(&doc, nj, "translation", "bad node.translation")?.unwrap_or([0.0; 3]);
        let rot_opt = fixed_f64::<4>(&doc, nj, "rotation", "bad node.rotation")?;
        let has_rotation = rot_opt.is_some();
        let rotation = rot_opt.unwrap_or([0.0, 0.0, 0.0, 1.0]);
        let scale = fixed_f64::<3>(&doc, nj, "scale", "bad node.scale")?.unwrap_or([1.0; 3]);

        let mut has_extras = false;
        let mut dequantize = None;
        let mut mass = None;
        let lod_start = count32(raw_lods.len())?;
        if let Some(ex) = doc.get(nj, "extras")
            && doc.as_obj(ex).is_some()
        {
            has_extras = true;
            if let Some(dq) = doc.get(ex, "dequantize") {
                let off = fixed_f64::<3>(&doc, dq, "translation", "bad extras.dequantize.translation")?.unwrap_or([0.0; 3]);
                let sc = fixed_f64::<3>(&doc, dq, "scale", "bad extras.dequantize.scale")?.unwrap_or([1.0; 3]);
                dequantize = Some((off, sc));
            }
            if let Some(m_val) = doc.get(ex, "mass").and_then(|v| doc.as_f64(v)) {
                let centre_gltf = fixed_f64::<3>(&doc, ex, "centre", "bad extras.centre")?.unwrap_or([0.0; 3]);
                let mut inertia_gltf = [[0.0; 3]; 3];
                if let Some(in_arr) = doc.get(ex, "inertia").and_then(|v| doc.as_arr(v))
                    && in_arr.len() == 3
                {
                    for (r, &row_id) in in_arr.iter().enumerate() {
                        let Some(row) = doc.as_arr(row_id) else {
                            return Err(GlbReadError::InvalidStructure { why: "bad extras.inertia row" });
                        };
                        if row.len() != 3 {
                            return Err(GlbReadError::InvalidStructure { why: "bad extras.inertia row length" });
                        }
                        for (c, &cell) in row.iter().enumerate() {
                            inertia_gltf[r][c] = doc
                                .as_f64(cell)
                                .ok_or(GlbReadError::InvalidStructure { why: "bad extras.inertia entry" })?;
                        }
                    }
                }
                mass = Some(MassFacts {
                    mass: m_val,
                    centre: unturn(centre_gltf, opts.up),
                    inertia: unturn_tensor(inertia_gltf, opts.up),
                });
            }
            for &lj in doc.list(ex, "lod") {
                let lm_u = doc
                    .get(lj, "mesh")
                    .and_then(|v| doc.as_u64(v))
                    .ok_or(GlbReadError::InvalidStructure { why: "extras.lod missing mesh" })?;
                let lm = ref_index(lm_u, meshes_meta.len(), "extras.lod.mesh")?;
                let error_m = doc
                    .get(lj, "error")
                    .and_then(|v| doc.as_f64(v))
                    .ok_or(GlbReadError::InvalidStructure { why: "extras.lod missing error" })?;
                let switch_m = doc
                    .get(lj, "switch_distance")
                    .and_then(|v| doc.as_f64(v))
                    .ok_or(GlbReadError::InvalidStructure { why: "extras.lod missing switch_distance" })?;
                raw_lods.try_push(RawLod { mesh: lm, error_m, switch_m }).map_err(|r| r.why)?;
            }
        }
        let lod_end = count32(raw_lods.len())?;

        raw_nodes
            .try_push(RawNode {
                name,
                children: (c_start, c_end),
                mesh,
                has_matrix,
                matrix,
                translation,
                rotation,
                scale,
                has_rotation,
                has_extras,
                dequantize,
                mass,
                lods: (lod_start, lod_end),
            })
            .map_err(|r| r.why)?;
    }

    // 8. Reconstruct scene forest in parent-before-child DFS pre-order.
    let scenes_json = doc.list(root, "scenes");
    let mut roots: V<'_, u32> = Vec::new_in(s);
    if !scenes_json.is_empty() {
        let scene_idx_u = match doc.get(root, "scene") {
            Some(v) => doc.as_u64(v).ok_or(GlbReadError::InvalidStructure { why: "scene not an integer" })?,
            None => 0,
        };
        let scene_idx = ref_index(scene_idx_u, scenes_json.len(), "scene")?;
        for &rj in doc.list(scenes_json[scene_idx], "nodes") {
            let ru = doc.as_u64(rj).ok_or(GlbReadError::InvalidStructure { why: "scenes.nodes item not an integer" })?;
            let ri = ref_index(ru, raw_nodes.len(), "scenes.nodes")?;
            roots.try_push(count32(ri)?).map_err(|r| r.why)?;
        }
    } else if !raw_nodes.is_empty() {
        // Fallback when `scenes` is omitted: every node with no parent is a root.
        let mut has_parent: V<'_, bool> = Vec::try_with_capacity_in(raw_nodes.len(), s)?;
        for _ in 0..raw_nodes.len() {
            has_parent.try_push(false).map_err(|r| r.why)?;
        }
        for &c in node_children.iter() {
            has_parent[idx(c)] = true;
        }
        for (i, &hp) in has_parent.iter().enumerate() {
            if !hp {
                roots.try_push(count32(i)?).map_err(|r| r.why)?;
            }
        }
    }

    let ctx = ReadCtx { bin, views: &views, accs: &accs, meshes: &meshes_meta, prims: &prims_meta, up: opts.up, scratch: s };
    let mut visited: V<'_, bool> = Vec::try_with_capacity_in(raw_nodes.len(), s)?;
    for _ in 0..raw_nodes.len() {
        visited.try_push(false).map_err(|r| r.why)?;
    }
    let mut stack: V<'_, (u32, Option<u32>)> = Vec::try_with_capacity_in(roots.len(), s)?;
    for &r in roots.iter().rev() {
        stack.try_push((r, None)).map_err(|r| r.why)?;
    }

    let mut parts: V<'a, LoadedPart<'a>> = Vec::new_in(a);
    while let Some((node_u, parent_part)) = stack.pop() {
        let ni = idx(node_u);
        if visited[ni] {
            return Err(GlbReadError::InvalidStructure { why: "node graph is not a forest (cycle or shared child)" });
        }
        visited[ni] = true;
        if parts.len() >= idx(opts.limits.most_parts) {
            return Err(GlbReadError::TooManyParts { parts: parts.len() + 1, most: opts.limits.most_parts });
        }
        let node = raw_nodes[ni];
        let kids = &node_children[idx(node.children.0)..idx(node.children.1)];

        // Detect a quantized `.mesh` dequantization child node and fold it into this part.
        let mut collapsed_slot: Option<usize> = None;
        let mut main_mesh_idx = node.mesh;
        let mut main_dequant = node.dequantize;
        if main_mesh_idx.is_none() {
            for (slot, &kid_u) in kids.iter().enumerate() {
                let kid = raw_nodes[idx(kid_u)];
                let is_mesh_child = kid.mesh.is_some()
                    && kid.children.0 == kid.children.1
                    && !kid.has_extras
                    && !kid.has_matrix
                    && !kid.has_rotation
                    && kid.name.strip_suffix(".mesh") == Some(node.name);
                if is_mesh_child {
                    collapsed_slot = Some(slot);
                    visited[idx(kid_u)] = true;
                    main_mesh_idx = kid.mesh;
                    main_dequant = Some((kid.translation, kid.scale));
                    break;
                }
            }
        }

        let part_idx_u = count32(parts.len())?;
        let part_id = PartId(part_idx_u);

        let transform = if node.has_matrix {
            Transform::Matrix(Matrix {
                linear: unturn_tensor(node.matrix.linear, opts.up),
                translation: unturn(node.matrix.translation, opts.up),
            })
        } else {
            Transform::Trs(Trs {
                translation: unturn(node.translation, opts.up),
                rotation: unturn_rotation(node.rotation, opts.up),
                scale: unturn_extent(node.scale, opts.up),
            })
        };

        let mesh = match main_mesh_idx {
            Some(mi) => Some(ctx.build_mesh(mi, part_id, main_dequant, a)?),
            None => None,
        };

        let lod_slice = &raw_lods[idx(node.lods.0)..idx(node.lods.1)];
        let mut lods: V<'a, LoadedLod<'a>> = Vec::try_with_capacity_in(lod_slice.len(), a)?;
        for &lod in lod_slice {
            let lod_mesh = ctx.build_mesh(lod.mesh, part_id, node.dequantize, a)?;
            lods.try_push(LoadedLod {
                mesh: lod_mesh,
                error: Length::m(lod.error_m),
                switch_distance: Length::m(lod.switch_m),
            })
            .map_err(|r| r.why)?;
        }

        let name = String::try_from_str_in(node.name, a)?;
        parts
            .try_push(LoadedPart { name, parent: parent_part, transform, mesh, lods, mass: node.mass })
            .map_err(|r| r.why)?;

        // Push remaining children in reverse so DFS visits them in original scene order.
        for (slot, &kid_u) in kids.iter().enumerate().rev() {
            if collapsed_slot != Some(slot) {
                stack.try_push((kid_u, Some(part_idx_u))).map_err(|r| r.why)?;
            }
        }
    }

    Ok(LoadedGlb { parts, materials, images })
}
