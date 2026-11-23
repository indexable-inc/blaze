//! The crate's promises, each an oracle, and the planted bugs each must catch:
//!
//! - every `.glb` exported from a drawn scene (float or quantized, Y or Z up, groups, levels of
//!   detail, colliders, joints) passes a validating parser written here from the glTF 2.0
//!   specification: the container and its chunks, 4-byte aligned views inside the buffer, each
//!   accessor inside its view, every `POSITION` `min`/`max` equal to the data's, indices in
//!   range, references in range, unit normals, the nodes a forest under the scene, extensions
//!   declared. Planted: `MisalignView` (`MisalignedView`), `AccessorPastView`
//!   (`AccessorPastView`), `WrongMinMax` (`MinMax`);
//! - the export says what the scene says: each part a node of its name under its parent's node,
//!   its transform turned to Y up exactly, its triangles and roles as given, its positions exact
//!   (float) or within the quantization's stated bound (quantized), its mass and levels in its
//!   extras; and two exports of one scene are byte-identical;
//! - one single-triangle `.glb` equals its expected bytes, written out below, byte for byte;
//! - every PNG decodes (by a decoder written here) to the pixels it was made from, every chunk's
//!   CRC-32 checked and `IEND`'s equal to the format's known constant. Planted: `CrcWithoutType`
//!   (`CrcMismatch`), `PaethTie` (`PixelsDiffer`);
//! - the summary equals a line per part formatted independently here. Planted: `SkipGroups`
//!   (`SummaryWrong`);
//! - every binary STL exported from a verified solid parses cleanly, avoids the `"solid "`
//!   header trap, carries canonical `+0.0` floats, unit outward normals matching right-hand
//!   winding, exact scaled vertices and positive signed volume; and a golden tetrahedron equals
//!   its expected 284 bytes byte for byte. Planted: `SolidHeader` (`AsciiHeader`),
//!   `WrongTriCount` (`LengthMismatch`), `FlipNormal` (`NormalDisagrees`), `FlipWinding`
//!   (`WindingMismatch`), `NegativeZero` (`NegativeZero`);
//! - every 2D vector SVG cross-section is well-formed XML/SVG 1.1 with `fill-rule="evenodd"`,
//!   closed `Z` silhouettes, Y-up mapped to SVG Y-down, exact saddle disambiguation (cases 5 &
//!   10), and circle/annulus perimeter & area matching `2πr` / `πr²` within tolerance. Planted:
//!   `WrongSaddle` (`WrongLoopCount`), `WrongInterp` (`GeometryError`), `DropZClose`
//!   (`UnclosedSilhouette`), `NoYFlip` (`WrongYSign`), `OmitEvenOdd` (`MissingEvenOdd`).

use core::fmt;
use std::string::{String, ToString};
use std::{format, vec, vec::Vec as StdVec};

use blaze_cad_mesh::geom::P3;
use blaze_cad_mesh::{CheckOpts, Columns, MaterialRole, MeshLimits, MeshView, PartId, TriId, check};
use blaze_data::Value;
use blaze_deflate::{Crc32, Level, MAX_RATIO, unzlib};
use blaze_own::Alloc;
use blaze_sim::{Broken, Config, Tape, controls};

use crate::*;

/// The test's allocator (D102): the process heap's handle, never `Global`.
fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn harness(e: impl fmt::Debug) -> Broken<OutBroken> {
    Broken::Harness(format!("{e:?}"))
}

/// A `u64` from the file as an index (a test on a 64-bit host: never cut).
fn at(x: u64) -> usize {
    usize::try_from(x).unwrap_or(usize::MAX)
}

/// Why the validating parser refused a `.glb`.
#[derive(Clone, PartialEq, Debug)]
enum Invalid {
    Container { why: &'static str },
    Json { why: String },
    Field { at: String, why: &'static str },
    Reference { at: String, index: u64, len: usize },
    BufferLength { declared: u64, chunk: usize },
    MisalignedView { view: usize, offset: u64 },
    ViewPastBuffer { view: usize, end: u64, buffer: u64 },
    Stride { view: usize, stride: u64 },
    AccessorPastView { accessor: usize, need: u64, view_length: u64 },
    MisalignedAccessor { accessor: usize },
    MinMax { accessor: usize, axis: usize, written: [f64; 2], actual: [f64; 2] },
    Index { accessor: usize, index: u64, vertices: u64 },
    NotUnit { accessor: usize, element: usize, length: f64 },
    Forest { node: usize, why: &'static str },
    Extension { name: String, why: &'static str },
}

/// Why the validating binary STL parser or mesh agreement oracle refused an `.stl`.
#[derive(Clone, PartialEq, Debug)]
enum StlInvalid {
    TooShort { len: usize },
    AsciiHeader,
    LengthMismatch { declared: u32, actual_bytes: usize },
    NonFinite { facet: usize, slot: usize },
    NegativeZero { facet: usize, slot: usize },
    NonZeroAttr { facet: usize, attr: u16 },
    NormalDisagrees { facet: usize, dot: f64 },
    WindingMismatch { facet: usize },
    VolumeWrong { volume: f64 },
}

/// Why the validating SVG parser or geometric oracle refused an `.svg`.
#[derive(Clone, PartialEq, Debug)]
enum SvgInvalid {
    Structure { why: &'static str },
    MissingEvenOdd,
    UnclosedSilhouette,
    WrongLoopCount { got: usize, want: usize },
    WrongYSign { got_cy: f64, want_cy: f64 },
    GeometryError { why: &'static str, got: f64, want: f64 },
}

#[derive(Clone, PartialEq, Debug)]
enum OutBroken {
    Invalid { why: Invalid },
    NotDeterministic { what: &'static str },
    PartWrong { part: usize, why: &'static str },
    PositionWrong { part: usize, vertex: usize, axis: usize, got: f64, want: f64, bound: f64 },
    GoldenDiffers { at: usize, got: Option<u8>, want: Option<u8> },
    PngInvalid { why: &'static str },
    CrcMismatch { chunk: [u8; 4] },
    PixelsDiffer { at: usize, got: u8, want: u8 },
    SummaryWrong { line: usize, got: String, want: String },
    StlInvalid { why: StlInvalid },
    SvgInvalid { why: SvgInvalid },
}

impl fmt::Display for OutBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

blaze_sim::plants! {
    impl GlbPlant breaks OutBroken {
        None,
        MisalignView => OutBroken::Invalid { why: Invalid::MisalignedView { .. } },
        AccessorPastView => OutBroken::Invalid { why: Invalid::AccessorPastView { .. } },
        WrongMinMax => OutBroken::Invalid { why: Invalid::MinMax { .. } },
    }
}

blaze_sim::plants! {
    impl PngPlant breaks OutBroken {
        None,
        CrcWithoutType => OutBroken::CrcMismatch { .. },
        PaethTie => OutBroken::PixelsDiffer { .. },
    }
}

blaze_sim::plants! {
    impl SummaryPlant breaks OutBroken {
        None,
        SkipGroups => OutBroken::SummaryWrong { .. },
    }
}

blaze_sim::plants! {
    impl StlPlant breaks OutBroken {
        None,
        SolidHeader => OutBroken::StlInvalid { why: StlInvalid::AsciiHeader },
        WrongTriCount => OutBroken::StlInvalid { why: StlInvalid::LengthMismatch { .. } },
        FlipNormal => OutBroken::StlInvalid { why: StlInvalid::NormalDisagrees { .. } },
        FlipWinding => OutBroken::StlInvalid { why: StlInvalid::WindingMismatch { .. } },
        NegativeZero => OutBroken::StlInvalid { why: StlInvalid::NegativeZero { .. } },
    }
}

blaze_sim::plants! {
    impl SvgPlant breaks OutBroken {
        None,
        WrongSaddle => OutBroken::SvgInvalid { why: SvgInvalid::WrongLoopCount { .. } },
        WrongInterp => OutBroken::SvgInvalid { why: SvgInvalid::GeometryError { .. } },
        DropZClose => OutBroken::SvgInvalid { why: SvgInvalid::UnclosedSilhouette },
        NoYFlip => OutBroken::SvgInvalid { why: SvgInvalid::WrongYSign { .. } },
        OmitEvenOdd => OutBroken::SvgInvalid { why: SvgInvalid::MissingEvenOdd },
    }
}

// ---------------------------------------------------------------------------------------------
// The validating glTF parser (Khronos, glTF 2.0 Specification), independent of the writer.

type J = Value<Alloc<'static>>;

const FLOAT: u64 = 5126;
const UNSIGNED_SHORT: u64 = 5123;
const UNSIGNED_INT: u64 = 5125;
const ARRAY_BUFFER: u64 = 34962;
const ELEMENT_ARRAY_BUFFER: u64 = 34963;
const KNOWN_EXTENSIONS: [&str; 3] = ["KHR_materials_clearcoat", "KHR_materials_emissive_strength", "KHR_mesh_quantization"];

struct View {
    offset: u64,
    length: u64,
    stride: Option<u64>,
    target: Option<u64>,
}

struct Acc {
    view: usize,
    offset: u64,
    component: u64,
    normalized: bool,
    count: u64,
    comps: u64,
}

struct Parsed {
    json: J,
    bin: StdVec<u8>,
    views: StdVec<View>,
    accs: StdVec<Acc>,
}

fn component_size(component: u64) -> Option<u64> {
    match component {
        5120 | 5121 => Some(1),
        5122 | 5123 => Some(2),
        5125 | 5126 => Some(4),
        _ => None,
    }
}

fn type_comps(kind: &str) -> Option<u64> {
    match kind {
        "SCALAR" => Some(1),
        "VEC2" => Some(2),
        "VEC3" => Some(3),
        "VEC4" => Some(4),
        _ => None,
    }
}

/// One component at byte `i` of `bytes`, raw (not normalized).
fn component(bytes: &[u8], i: usize, component: u64) -> f64 {
    match component {
        5120 => f64::from(i8::from_le_bytes([bytes[i]])),
        5121 => f64::from(bytes[i]),
        5122 => f64::from(i16::from_le_bytes([bytes[i], bytes[i + 1]])),
        5123 => f64::from(u16::from_le_bytes([bytes[i], bytes[i + 1]])),
        5125 => f64::from(u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]])),
        _ => f64::from(f32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]])),
    }
}

fn list<'v>(v: &'v J, key: &str) -> &'v [J] {
    v.get(key).and_then(|x| x.as_list()).unwrap_or(&[])
}

fn whole(v: &J, key: &str, at: &str) -> Result<u64, Invalid> {
    v.get(key)
        .and_then(|x| x.as_u64())
        .ok_or_else(|| Invalid::Field { at: format!("{at}.{key}"), why: "missing or not a whole number" })
}

fn whole_or(v: &J, key: &str, default: u64, at: &str) -> Result<u64, Invalid> {
    match v.get(key) {
        None => Ok(default),
        Some(x) => x.as_u64().ok_or_else(|| Invalid::Field { at: format!("{at}.{key}"), why: "not a whole number" }),
    }
}

fn numbers(v: &J, key: &str, at: &str) -> Result<Option<StdVec<f64>>, Invalid> {
    let Some(x) = v.get(key) else { return Ok(None) };
    let bad = || Invalid::Field { at: format!("{at}.{key}"), why: "not a list of numbers" };
    let items = x.as_list().ok_or_else(bad)?;
    items.iter().map(|n| n.as_f64().ok_or_else(bad)).collect::<Result<StdVec<f64>, Invalid>>().map(Some)
}

fn numbers_or(v: &J, key: &str, default: &[f64]) -> StdVec<f64> {
    numbers(v, key, "").ok().flatten().unwrap_or_else(|| default.to_vec())
}

fn reference(index: u64, len: usize, at: String) -> Result<usize, Invalid> {
    let i = self::at(index);
    if i < len { Ok(i) } else { Err(Invalid::Reference { at, index, len }) }
}

fn in_unit(xs: &[f64]) -> bool {
    xs.iter().all(|&x| (0.0..=1.0).contains(&x))
}

impl Parsed {
    fn list(&self, key: &str) -> &[J] {
        list(&self.json, key)
    }

    /// Element `i` of accessor `a`, each component raw.
    fn element(&self, a: usize, i: u64) -> StdVec<f64> {
        let acc = &self.accs[a];
        let view = &self.views[acc.view];
        let size = component_size(acc.component).unwrap_or(1);
        let stride = view.stride.unwrap_or(acc.comps * size);
        let start = view.offset + acc.offset + stride * i;
        (0..acc.comps).map(|k| component(&self.bin, at(start + k * size), acc.component)).collect()
    }

    /// Element `i` of a vector accessor as its value: normalized integers mapped to `[-1, 1]`.
    fn vector(&self, a: usize, i: u64) -> StdVec<f64> {
        let acc = &self.accs[a];
        let raw = self.element(a, i);
        if !acc.normalized {
            return raw;
        }
        let m = match acc.component {
            5120 => 127.0,
            5121 => 255.0,
            5122 => 32767.0,
            _ => 65535.0,
        };
        raw.into_iter().map(|c| (c / m).max(-1.0)).collect()
    }
}

/// The container: header, the JSON chunk, the BIN chunk.
fn container(bytes: &[u8]) -> Result<Parsed, Invalid> {
    let c = |why| Invalid::Container { why };
    if bytes.len() < 20 {
        return Err(c("shorter than a header and a chunk header"));
    }
    let u32_at = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    if &bytes[0..4] != b"glTF" {
        return Err(c("magic is not glTF"));
    }
    if u32_at(4) != 2 {
        return Err(c("version is not 2"));
    }
    if blaze_int::idx(u32_at(8)) != bytes.len() {
        return Err(c("header length is not the file's"));
    }
    let json_len = blaze_int::idx(u32_at(12));
    if &bytes[16..20] != b"JSON" {
        return Err(c("first chunk is not JSON"));
    }
    if json_len % 4 != 0 || json_len == 0 {
        return Err(c("JSON chunk length is not a positive multiple of 4"));
    }
    let json_end = 20 + json_len;
    if json_end > bytes.len() {
        return Err(c("JSON chunk past the file"));
    }
    let mut bin = StdVec::new();
    if json_end < bytes.len() {
        if bytes.len() - json_end < 8 {
            return Err(c("a cut chunk header"));
        }
        let bin_len = blaze_int::idx(u32_at(json_end));
        if &bytes[json_end + 4..json_end + 8] != b"BIN\0" {
            return Err(c("second chunk is not BIN"));
        }
        if bin_len % 4 != 0 || json_end + 8 + bin_len != bytes.len() {
            return Err(c("BIN chunk length is not a multiple of 4 ending the file"));
        }
        bin = bytes[json_end + 8..].to_vec();
    }
    let json = blaze_json::read_in(&bytes[20..json_end], blaze_json::Limits::SHIPPED, heap())
        .map_err(|e| Invalid::Json { why: format!("{e:?}") })?;
    if json.as_map().is_none() {
        return Err(c("the JSON is not an object"));
    }
    Ok(Parsed { json, bin, views: StdVec::new(), accs: StdVec::new() })
}

fn parse(bytes: &[u8]) -> Result<Parsed, Invalid> {
    let mut g = container(bytes)?;
    let asset = g.json.get("asset").ok_or(Invalid::Field { at: "asset".to_string(), why: "missing" })?;
    if asset.get("version").and_then(|v| v.as_str()) != Some("2.0") {
        return Err(Invalid::Field { at: "asset.version".to_string(), why: "not 2.0" });
    }

    // Buffers: one, the BIN chunk, at most three bytes of padding past its length.
    let buffers = g.list("buffers");
    let buffer_len = match (buffers.len(), g.bin.is_empty()) {
        (0, true) => 0,
        (1, false) => {
            let declared = whole(&buffers[0], "byteLength", "buffers[0]")?;
            if declared > g.bin.len() as u64 || g.bin.len() as u64 - declared > 3 {
                return Err(Invalid::BufferLength { declared, chunk: g.bin.len() });
            }
            if buffers[0].get("uri").is_some() {
                return Err(Invalid::Field { at: "buffers[0].uri".to_string(), why: "the GLB buffer has no uri" });
            }
            declared
        }
        _ => return Err(Invalid::BufferLength { declared: buffers.len() as u64, chunk: g.bin.len() }),
    };

    // Views: inside the buffer, 4-byte aligned, a stride of 4 to 252 in steps of 4.
    let mut views = StdVec::new();
    for (v, view) in g.list("bufferViews").iter().enumerate() {
        let here = format!("bufferViews[{v}]");
        reference(whole(view, "buffer", &here)?, buffers.len(), format!("{here}.buffer"))?;
        let offset = whole_or(view, "byteOffset", 0, &here)?;
        let length = whole(view, "byteLength", &here)?;
        if offset % 4 != 0 {
            return Err(Invalid::MisalignedView { view: v, offset });
        }
        if length == 0 || offset + length > buffer_len {
            return Err(Invalid::ViewPastBuffer { view: v, end: offset + length, buffer: buffer_len });
        }
        let stride = match view.get("byteStride") {
            None => None,
            Some(_) => {
                let s = whole(view, "byteStride", &here)?;
                if !(4..=252).contains(&s) || s % 4 != 0 {
                    return Err(Invalid::Stride { view: v, stride: s });
                }
                Some(s)
            }
        };
        let target = match view.get("target") {
            None => None,
            Some(_) => {
                let t = whole(view, "target", &here)?;
                if t != ARRAY_BUFFER && t != ELEMENT_ARRAY_BUFFER {
                    return Err(Invalid::Field { at: format!("{here}.target"), why: "not a buffer target" });
                }
                Some(t)
            }
        };
        views.push(View { offset, length, stride, target });
    }
    g.views = views;

    // Accessors: each inside its view and aligned to its component; then min and max, where
    // written, equal to the data's.
    let mut accs = StdVec::new();
    for (a, acc) in g.list("accessors").iter().enumerate() {
        let here = format!("accessors[{a}]");
        if acc.get("sparse").is_some() {
            return Err(Invalid::Field { at: here, why: "sparse accessors are not written" });
        }
        let view = reference(whole(acc, "bufferView", &here)?, g.views.len(), format!("{here}.bufferView"))?;
        let component = whole(acc, "componentType", &here)?;
        let size = component_size(component).ok_or(Invalid::Field { at: format!("{here}.componentType"), why: "unknown" })?;
        let comps = acc
            .get("type")
            .and_then(|t| t.as_str())
            .and_then(type_comps)
            .ok_or(Invalid::Field { at: format!("{here}.type"), why: "unknown" })?;
        let count = whole(acc, "count", &here)?;
        if count == 0 {
            return Err(Invalid::Field { at: format!("{here}.count"), why: "zero" });
        }
        let offset = whole_or(acc, "byteOffset", 0, &here)?;
        let normalized = acc.get("normalized").map_or(Some(false), |n| n.as_bool()).unwrap_or(false);
        let v = &g.views[view];
        let element = comps * size;
        let stride = v.stride.unwrap_or(element);
        if stride < element {
            return Err(Invalid::Stride { view, stride });
        }
        let need = offset + stride * (count - 1) + element;
        if need > v.length {
            return Err(Invalid::AccessorPastView { accessor: a, need, view_length: v.length });
        }
        if (v.offset + offset) % size != 0 || stride % size != 0 {
            return Err(Invalid::MisalignedAccessor { accessor: a });
        }
        accs.push(Acc { view, offset, component, normalized, count, comps });
    }
    g.accs = accs;
    for (a, acc) in g.list("accessors").iter().enumerate() {
        let here = format!("accessors[{a}]");
        let (min, max) = (numbers(acc, "min", &here)?, numbers(acc, "max", &here)?);
        let (min, max) = match (min, max) {
            (None, None) => continue,
            (Some(min), Some(max)) => (min, max),
            _ => return Err(Invalid::Field { at: here, why: "min without max or max without min" }),
        };
        let comps = g.accs[a].comps;
        if min.len() as u64 != comps || max.len() as u64 != comps {
            return Err(Invalid::Field { at: here, why: "min or max of the wrong length" });
        }
        let mut lo = vec![f64::INFINITY; at(comps)];
        let mut hi = vec![f64::NEG_INFINITY; at(comps)];
        for i in 0..g.accs[a].count {
            for (k, x) in g.element(a, i).into_iter().enumerate() {
                lo[k] = lo[k].min(x);
                hi[k] = hi[k].max(x);
            }
        }
        for k in 0..at(comps) {
            if min[k] != lo[k] || max[k] != hi[k] {
                return Err(Invalid::MinMax { accessor: a, axis: k, written: [min[k], max[k]], actual: [lo[k], hi[k]] });
            }
        }
    }

    // Meshes: POSITION with min and max, attributes of one count, unit normals, indices in
    // range, materials in range.
    let materials = g.list("materials").len();
    let mut quantized = false;
    for (m, mesh) in g.list("meshes").iter().enumerate() {
        let prims = list(mesh, "primitives");
        if prims.is_empty() {
            return Err(Invalid::Field { at: format!("meshes[{m}].primitives"), why: "empty" });
        }
        for (p, prim) in prims.iter().enumerate() {
            let here = format!("meshes[{m}].primitives[{p}]");
            let attrs = prim
                .get("attributes")
                .and_then(|x| x.as_map())
                .ok_or(Invalid::Field { at: here.clone(), why: "no attributes" })?;
            let position = attrs.get("POSITION").ok_or(Invalid::Field { at: here.clone(), why: "no POSITION" })?;
            let position = reference(position.as_u64().unwrap_or(u64::MAX), g.accs.len(), format!("{here}.POSITION"))?;
            let vertices = g.accs[position].count;
            for (name, value) in attrs.iter() {
                let a = reference(value.as_u64().unwrap_or(u64::MAX), g.accs.len(), format!("{here}.{name}"))?;
                let acc = &g.accs[a];
                if acc.count != vertices {
                    return Err(Invalid::Field { at: format!("{here}.{name}"), why: "a count unlike POSITION's" });
                }
                if g.views[acc.view].target.is_some_and(|t| t != ARRAY_BUFFER) {
                    return Err(Invalid::Field { at: format!("{here}.{name}"), why: "a vertex view not ARRAY_BUFFER" });
                }
                if acc.component != FLOAT {
                    quantized = true;
                }
                match name {
                    "POSITION" => {
                        if acc.comps != 3 || (acc.component != FLOAT && acc.normalized) {
                            return Err(Invalid::Field { at: format!("{here}.POSITION"), why: "not VEC3 of float or integers" });
                        }
                        if g.json.get("accessors").and_then(|x| x.as_list()).is_some_and(|l| l[a].get("min").is_none()) {
                            return Err(Invalid::Field { at: format!("{here}.POSITION"), why: "no min and max" });
                        }
                    }
                    "NORMAL" => {
                        let normal_type = acc.component == FLOAT || (acc.normalized && (acc.component == 5120 || acc.component == 5122));
                        if acc.comps != 3 || !normal_type {
                            return Err(Invalid::Field { at: format!("{here}.NORMAL"), why: "not VEC3 of float or normalized signed" });
                        }
                        let tolerance = if acc.component == FLOAT { 1e-5 } else { 0.01 };
                        for i in 0..acc.count {
                            let n = g.vector(a, i);
                            let length = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
                            if (length - 1.0).abs() > tolerance {
                                return Err(Invalid::NotUnit { accessor: a, element: at(i), length });
                            }
                        }
                    }
                    "TEXCOORD_0" => {
                        if acc.comps != 2 || acc.component != FLOAT || acc.normalized {
                            return Err(Invalid::Field { at: format!("{here}.TEXCOORD_0"), why: "not VEC2 of float" });
                        }
                    }
                    "TANGENT" => {
                        if acc.comps != 4 || acc.component != FLOAT || acc.normalized {
                            return Err(Invalid::Field { at: format!("{here}.TANGENT"), why: "not VEC4 of float" });
                        }
                        for i in 0..acc.count {
                            let t = g.vector(a, i);
                            let length = (t[0] * t[0] + t[1] * t[1] + t[2] * t[2]).sqrt();
                            if (length - 1.0).abs() > 1e-5 {
                                return Err(Invalid::NotUnit { accessor: a, element: at(i), length });
                            }
                            if t[3] != 1.0 && t[3] != -1.0 {
                                return Err(Invalid::Field { at: format!("{here}.TANGENT"), why: "handedness w not +-1" });
                            }
                        }
                    }
                    _ => return Err(Invalid::Field { at: format!("{here}.{name}"), why: "an attribute this writer never writes" }),
                }
            }
            let indices = reference(whole(prim, "indices", &here)?, g.accs.len(), format!("{here}.indices"))?;
            let acc = &g.accs[indices];
            if acc.comps != 1 || acc.normalized || !(acc.component == 5121 || acc.component == UNSIGNED_SHORT || acc.component == UNSIGNED_INT)
            {
                return Err(Invalid::Field { at: format!("{here}.indices"), why: "not unsigned SCALAR" });
            }
            if acc.count % 3 != 0 {
                return Err(Invalid::Field { at: format!("{here}.indices"), why: "not whole triangles" });
            }
            if g.views[acc.view].target.is_some_and(|t| t != ELEMENT_ARRAY_BUFFER) {
                return Err(Invalid::Field { at: format!("{here}.indices"), why: "an index view not ELEMENT_ARRAY_BUFFER" });
            }
            for i in 0..acc.count {
                let index = g.element(indices, i)[0] as u64;
                if index >= vertices {
                    return Err(Invalid::Index { accessor: indices, index, vertices });
                }
            }
            if let Some(material) = prim.get("material") {
                reference(material.as_u64().unwrap_or(u64::MAX), materials, format!("{here}.material"))?;
            }
        }
    }

    // Nodes: a forest whose roots are the scene's, every node in it; transforms well formed.
    let nodes = g.list("nodes");
    let n = nodes.len();
    let meshes = g.list("meshes").len();
    let mut parent: StdVec<Option<usize>> = vec![None; n];
    for (i, node) in nodes.iter().enumerate() {
        let here = format!("nodes[{i}]");
        for child in list(node, "children") {
            let c = reference(child.as_u64().unwrap_or(u64::MAX), n, format!("{here}.children"))?;
            if c == i || parent[c].is_some() {
                return Err(Invalid::Forest { node: c, why: "its own child, or a child twice" });
            }
            parent[c] = Some(i);
        }
        if let Some(mesh) = node.get("mesh") {
            reference(mesh.as_u64().unwrap_or(u64::MAX), meshes, format!("{here}.mesh"))?;
        }
        if numbers(node, "translation", &here)?.is_some_and(|t| t.len() != 3) {
            return Err(Invalid::Field { at: here, why: "translation not of 3" });
        }
        if let Some(r) = numbers(node, "rotation", &here)? {
            let length = r.iter().map(|x| x * x).sum::<f64>().sqrt();
            if r.len() != 4 || (length - 1.0).abs() > 1e-6 {
                return Err(Invalid::Field { at: here, why: "rotation not a unit quaternion" });
            }
        }
        if numbers(node, "scale", &here)?.is_some_and(|s| s.len() != 3) {
            return Err(Invalid::Field { at: here, why: "scale not of 3" });
        }
    }
    let root_of = |mut i: usize| -> Option<usize> {
        for _ in 0..=n {
            match parent[i] {
                Some(p) => i = p,
                None => return Some(i),
            }
        }
        None
    };
    let scenes = g.list("scenes");
    let scene = reference(whole_or(&g.json, "scene", 0, "")?, scenes.len(), "scene".to_string())?;
    let mut roots = StdVec::new();
    for r in list(&scenes[scene], "nodes") {
        let r = reference(r.as_u64().unwrap_or(u64::MAX), n, "scenes.nodes".to_string())?;
        if parent[r].is_some() {
            return Err(Invalid::Forest { node: r, why: "a scene root with a parent" });
        }
        roots.push(r);
    }
    for i in 0..n {
        match root_of(i) {
            None => return Err(Invalid::Forest { node: i, why: "on a cycle" }),
            Some(r) if !roots.contains(&r) => return Err(Invalid::Forest { node: i, why: "outside the scene" }),
            Some(_) => {}
        }
    }

    // Extensions: known, required ones used, every one in use declared.
    let used: StdVec<&str> = g.list("extensionsUsed").iter().filter_map(|x| x.as_str()).collect();
    let required: StdVec<&str> = g.list("extensionsRequired").iter().filter_map(|x| x.as_str()).collect();
    for name in &used {
        if !KNOWN_EXTENSIONS.contains(name) {
            return Err(Invalid::Extension { name: name.to_string(), why: "unknown" });
        }
    }
    for name in &required {
        if !used.contains(name) {
            return Err(Invalid::Extension { name: name.to_string(), why: "required but not used" });
        }
    }
    if quantized && !required.contains(&"KHR_mesh_quantization") {
        return Err(Invalid::Extension { name: "KHR_mesh_quantization".to_string(), why: "needed but not required" });
    }
    // Images, samplers, and textures.
    let images_len = g.list("images").len();
    for (im, image) in g.list("images").iter().enumerate() {
        let here = format!("images[{im}]");
        let bv = reference(whole(image, "bufferView", &here)?, g.views.len(), format!("{here}.bufferView"))?;
        if g.views[bv].target.is_some() {
            return Err(Invalid::Field { at: format!("{here}.bufferView"), why: "image bufferView must omit target" });
        }
        if image.get("mimeType").and_then(|v| v.as_str()) != Some("image/png") {
            return Err(Invalid::Field { at: format!("{here}.mimeType"), why: "not image/png" });
        }
    }
    let samplers_len = g.list("samplers").len();
    let textures_len = g.list("textures").len();
    for (ti, tex) in g.list("textures").iter().enumerate() {
        let here = format!("textures[{ti}]");
        reference(whole(tex, "source", &here)?, images_len, format!("{here}.source"))?;
        if let Some(s) = tex.get("sampler") {
            reference(s.as_u64().unwrap_or(u64::MAX), samplers_len, format!("{here}.sampler"))?;
        }
    }

    for (m, material) in g.list("materials").iter().enumerate() {
        let here = format!("materials[{m}]");
        let bad = |why| Invalid::Field { at: here.clone(), why };
        if let Some(pbr) = material.get("pbrMetallicRoughness") {
            if !numbers(pbr, "baseColorFactor", &here)?.is_none_or(|c| c.len() == 4 && in_unit(&c)) {
                return Err(bad("baseColorFactor"));
            }
            for key in ["metallicFactor", "roughnessFactor"] {
                if !pbr.get(key).is_none_or(|x| x.as_f64().is_some_and(|x| in_unit(&[x]))) {
                    return Err(bad("a factor outside 0 to 1"));
                }
            }
            for key in ["baseColorTexture", "metallicRoughnessTexture"] {
                if let Some(info) = pbr.get(key) {
                    let ti = whole(info, "index", &format!("{here}.{key}"))?;
                    reference(ti, textures_len, format!("{here}.{key}.index"))?;
                }
            }
        }
        if let Some(nt) = material.get("normalTexture") {
            let ti = whole(nt, "index", &format!("{here}.normalTexture"))?;
            reference(ti, textures_len, format!("{here}.normalTexture.index"))?;
            if let Some(sc) = nt.get("scale")
                && !sc.as_f64().is_some_and(|x| x.is_finite() && x >= 0.0)
            {
                return Err(bad("normalTexture.scale"));
            }
        }
        if !numbers(material, "emissiveFactor", &here)?.is_none_or(|e| e.len() == 3 && in_unit(&e)) {
            return Err(bad("emissiveFactor"));
        }
        if let Some(extensions) = material.get("extensions").and_then(|x| x.as_map()) {
            for (name, _) in extensions.iter() {
                if !used.contains(&name) {
                    return Err(Invalid::Extension { name: name.to_string(), why: "in use but not declared" });
                }
            }
        }
    }
    Ok(g)
}

// ---------------------------------------------------------------------------------------------
// Scenes the tests own.

const NORMALS: [P3; 8] = [
    [1.0, 0.0, 0.0],
    [-1.0, 0.0, 0.0],
    [0.0, 1.0, 0.0],
    [0.0, -1.0, 0.0],
    [0.0, 0.0, 1.0],
    [0.0, 0.0, -1.0],
    [0.0, 3.0, 4.0],
    [1.0, 1.0, 1.0],
];

const ROTATIONS: [[f64; 4]; 4] = [[0.0, 0.0, 0.0, 1.0], [0.0, 0.0, 1.0, 0.0], [0.6, 0.0, 0.0, 0.8], [0.5, 0.5, 0.5, 0.5]];

const SCALES: [[f64; 3]; 3] = [[1.0, 1.0, 1.0], [2.0, 0.5, 3.0], [0.25, 4.0, 1.0]];

const KINDS: [JointKind; 4] = [JointKind::Fixed, JointKind::Revolute, JointKind::Prismatic, JointKind::Ball];

/// A number on a 1/16 grid in `[-10, 10]`: exact in decimal, so short in JSON.
fn sixteenth(tape: &Tape, label: &'static str) -> f64 {
    f64::from(tape.int_in(-160i32..=160, label)) / 16.0
}

/// A mesh's columns, owned: random triangles over 3 to 12 vertices, x spanning `[-5, 5]` so
/// every mesh has extent on x (a planted `max = min` on x is then always wrong).
struct Soup {
    positions: StdVec<P3>,
    normals: StdVec<P3>,
    triangles: StdVec<[u32; 3]>,
    parts: StdVec<PartId>,
    roles: StdVec<MaterialRole>,
}

impl Soup {
    fn draw(tape: &Tape, roles: u64) -> Soup {
        let mut positions =
            tape.vec(3..=12, "vertex", |t| [0, 1, 2].map(|_| f32::from(t.int_in(-256i16..=256, "coordinate")) / 64.0));
        positions[0][0] = -5.0;
        positions[1][0] = 5.0;
        let normals = positions.iter().map(|_| *tape.pick(&NORMALS, "normal").unwrap_or(&NORMALS[4])).collect();
        let nv = positions.len();
        let triangles = tape.vec(1..=8, "triangle", |t| [0, 1, 2].map(|_| u32::try_from(t.index(nv, "corner")).unwrap_or(0)));
        let roles = triangles.iter().map(|_| MaterialRole(u32::try_from(tape.below(roles, "role")).unwrap_or(0))).collect();
        let parts = vec![PartId(0); triangles.len()];
        Soup { positions, normals, triangles, parts, roles }
    }

    fn view(&self) -> Result<MeshView<'_>, Broken<OutBroken>> {
        MeshView::new(Columns {
            positions: &self.positions,
            normals: &self.normals,
            tangents: &[],
            uvs: &[],
            colours: &[],
            triangles: &self.triangles,
            parts: &self.parts,
            roles: &self.roles,
            creases: &[],
        })
        .map_err(harness)
    }
}

#[derive(Clone, Copy)]
enum ColliderPick {
    Cuboid,
    Ball,
    OwnMesh,
    Hulls(usize),
}

struct PartSpec {
    name: String,
    parent: Option<u32>,
    transform: Transform,
    mesh: Option<usize>,
    lod: Option<usize>,
    mass: Option<MassFacts>,
    joint: Option<(JointKind, Option<[f64; 2]>)>,
    collider: Option<ColliderPick>,
}

struct Drawn {
    soups: StdVec<Soup>,
    parts: StdVec<PartSpec>,
}

fn keep(soups: &mut StdVec<Soup>, soup: Soup) -> usize {
    soups.push(soup);
    soups.len() - 1
}

fn materials() -> [Material<'static>; 3] {
    [
        Material {
            name: "paint.hull",
            base_colour: [0.8, 0.1, 0.1, 1.0],
            metallic: 0.25,
            roughness: 0.5,
            emissive: [0.0; 3],
            emissive_strength: None,
            clearcoat: Some(Clearcoat { factor: 1.0, roughness: 0.125 }),
            double_sided: false,
        },
        Material {
            name: "glass.canopy",
            base_colour: [0.5, 0.75, 0.875, 0.25],
            metallic: 0.0,
            roughness: 0.0625,
            emissive: [0.0; 3],
            emissive_strength: None,
            clearcoat: None,
            double_sided: true,
        },
        Material {
            name: "light.nav",
            base_colour: [1.0; 4],
            metallic: 0.0,
            roughness: 1.0,
            emissive: [1.0, 0.25, 0.25],
            emissive_strength: Some(4.0),
            clearcoat: None,
            double_sided: false,
        },
    ]
}

impl Drawn {
    /// One to six parts; the first always has a mesh, later ones may be groups.
    fn draw(tape: &Tape) -> Drawn {
        let mut soups = StdVec::new();
        let mut parts = StdVec::new();
        let count = tape.int_in(1..=6usize, "parts");
        for i in 0..count {
            let name = if tape.one_in(4, "awkward name") { format!("p\t{i}\"") } else { format!("p{i}") };
            let parent =
                if i > 0 && tape.coin("has a parent") { Some(u32::try_from(tape.index(i, "parent")).unwrap_or(0)) } else { None };
            let translation = [0, 1, 2].map(|_| sixteenth(tape, "translation"));
            let transform = if tape.one_in(4, "mirrored placement") {
                // A mirror across one axis, then a quarter turn about another: det −1, no TRS.
                let axis = tape.index(3, "mirror axis");
                let mut linear = [[0.0; 3]; 3];
                let turn_of = [1usize, 2, 0];
                for r in 0..3 {
                    linear[r][turn_of[r]] = if r == axis { -1.0 } else { 1.0 };
                }
                Transform::Matrix(Matrix { linear, translation })
            } else {
                Transform::Trs(Trs {
                    translation,
                    rotation: *tape.pick(&ROTATIONS, "rotation").unwrap_or(&ROTATIONS[0]),
                    scale: *tape.pick(&SCALES, "scale").unwrap_or(&SCALES[0]),
                })
            };
            let mesh = if i == 0 || tape.coin("has a mesh") { Some(keep(&mut soups, Soup::draw(tape, 3))) } else { None };
            let lod = if mesh.is_some() && tape.coin("has a level") { Some(keep(&mut soups, Soup::draw(tape, 3))) } else { None };
            let mass = if tape.coin("has mass") {
                let (a, b, c) = (sixteenth(tape, "inertia").abs(), sixteenth(tape, "inertia").abs(), sixteenth(tape, "inertia").abs());
                let (d, e, f) = (sixteenth(tape, "product"), sixteenth(tape, "product"), sixteenth(tape, "product"));
                Some(MassFacts {
                    mass: sixteenth(tape, "mass").abs(),
                    centre: [0, 1, 2].map(|_| sixteenth(tape, "centre")),
                    inertia: [[a, d, e], [d, b, f], [e, f, c]],
                })
            } else {
                None
            };
            let joint = if parent.is_some() && tape.coin("has a joint") {
                let kind = *tape.pick(&KINDS, "joint kind").unwrap_or(&JointKind::Fixed);
                Some((kind, if tape.coin("limited") { Some([-1.5, 1.5]) } else { None }))
            } else {
                None
            };
            let collider = match tape.below(5, "collider") {
                0 => None,
                1 => Some(ColliderPick::Cuboid),
                2 => Some(ColliderPick::Ball),
                3 if mesh.is_some() => Some(ColliderPick::OwnMesh),
                3 => None,
                _ => Some(ColliderPick::Hulls(keep(&mut soups, Soup::draw(tape, 1)))),
            };
            parts.push(PartSpec { name, parent, transform, mesh, lod, mass, joint, collider });
        }
        Drawn { soups, parts }
    }

    /// `body` run on the scene these columns make.
    fn with_scene<R>(&self, body: impl FnOnce(&Scene<'_>) -> Result<R, Broken<OutBroken>>) -> Result<R, Broken<OutBroken>> {
        let views = self.soups.iter().map(Soup::view).collect::<Result<StdVec<MeshView<'_>>, _>>()?;
        let lods: StdVec<StdVec<Lod<'_>>> = self
            .parts
            .iter()
            .map(|p| p.lod.map(|s| vec![Lod { mesh: views[s], error: blaze_units::Length::mm(125.0), switch_distance: blaze_units::Length::m(20.0) }]).unwrap_or_default())
            .collect();
        let hulls: StdVec<StdVec<MeshView<'_>>> = self
            .parts
            .iter()
            .map(|p| match p.collider {
                Some(ColliderPick::Hulls(s)) => vec![views[s]],
                _ => StdVec::new(),
            })
            .collect();
        let parts: StdVec<Part<'_>> = self
            .parts
            .iter()
            .enumerate()
            .map(|(i, p)| Part {
                name: &p.name,
                parent: p.parent,
                transform: p.transform,
                mesh: p.mesh.map(|s| views[s]),
                lods: &lods[i],
                mass: p.mass,
                joint: p.joint.map(|(kind, limits)| Joint { name: "hinge", kind, axis: [0.0, 0.0, 1.0], limits }),
                collider: p.collider.map(|c| match c {
                    ColliderPick::Cuboid => Collider::Box { centre: [0.0, 0.5, 0.0], half: [1.0, 2.0, 0.5] },
                    ColliderPick::Ball => Collider::Sphere { centre: [0.0; 3], radius: 1.5 },
                    ColliderPick::OwnMesh => Collider::Mesh,
                    ColliderPick::Hulls(_) => Collider::Hulls(&hulls[i]),
                }),
            })
            .collect();
        let materials = materials();
        body(&Scene { parts: &parts, materials: &materials })
    }
}

// ---------------------------------------------------------------------------------------------
// The export says what the scene says.

/// glTF's frame from the scene's, written here apart from the crate's own.
fn turned(v: [f64; 3], up: Up) -> [f64; 3] {
    match up {
        Up::Y => v,
        Up::Z => [v[0], v[2], -v[1]],
    }
}

/// [`turned`]'s inverse: glTF's frame back to the scene's.
fn unturned(v: [f64; 3], up: Up) -> [f64; 3] {
    match up {
        Up::Y => v,
        Up::Z => [v[0], -v[2], v[1]],
    }
}

fn turned_extent(v: [f64; 3], up: Up) -> [f64; 3] {
    match up {
        Up::Y => v,
        Up::Z => [v[0], v[2], v[1]],
    }
}

fn turned_f32(p: P3, up: Up) -> P3 {
    match up {
        Up::Y => p,
        Up::Z => [p[0], p[2], -p[1]],
    }
}

fn node_named(nodes: &[J], name: &str) -> Option<usize> {
    nodes.iter().position(|n| n.get("name").and_then(|x| x.as_str()) == Some(name))
}

fn children(node: &J) -> StdVec<usize> {
    list(node, "children").iter().filter_map(|c| c.as_u64()).map(at).collect()
}

/// The quantization's stated bound on one axis (blaze-cad-mesh's quant module), from the
/// dequantization the node carries, with a millionth to spare for the scale's own rounding.
fn quant_bound(offset: f64, scale: f64) -> f64 {
    let extent = scale * 65535.0;
    let m = offset.abs().max((offset + extent).abs());
    let ulp24 = 1.0 / 16_777_216.0;
    (scale * 0.5 + 65535.0 * ulp24 * scale + 4.0 * ulp24 * (extent + m) + (extent + m) / 1_099_511_627_776.0) * (1.0 + 1e-6)
}

fn agrees(g: &Parsed, scene: &Scene<'_>, encoding: Encoding, up: Up) -> Result<(), Broken<OutBroken>> {
    let wrong = |part: usize, why: &'static str| Broken::Oracle(OutBroken::PartWrong { part, why });
    let quantized = matches!(encoding, Encoding::Quantized { .. });
    let nodes = g.list("nodes");
    let meshes = g.list("meshes");
    let scenes = g.list("scenes");
    let roots = scenes.first().map(children_of_scene).unwrap_or_default();
    for (i, part) in scene.parts.iter().enumerate() {
        let Some(n) = node_named(nodes, part.name) else { return Err(wrong(i, "no node of the part's name")) };
        let node = &nodes[n];
        let extras = node.get("extras").ok_or_else(|| wrong(i, "no extras"))?;
        if extras.get("name").and_then(|x| x.as_str()) != Some(part.name) {
            return Err(wrong(i, "extras name"));
        }
        match part.parent {
            Some(p) => {
                let Some(pn) = node_named(nodes, scene.parts[blaze_int::idx(p)].name) else {
                    return Err(wrong(i, "no node for the parent"));
                };
                if !children(&nodes[pn]).contains(&n) {
                    return Err(wrong(i, "not a child of its parent's node"));
                }
            }
            None => {
                if !roots.contains(&n) {
                    return Err(wrong(i, "a root part not a scene root"));
                }
            }
        }
        match part.transform {
            Transform::Trs(t) => {
                if !numbers_or(node, "matrix", &[]).is_empty() {
                    return Err(wrong(i, "matrix on a TRS node"));
                }
                if numbers_or(node, "translation", &[0.0; 3])[..] != turned(t.translation, up)[..] {
                    return Err(wrong(i, "translation"));
                }
                let v = turned([t.rotation[0], t.rotation[1], t.rotation[2]], up);
                if numbers_or(node, "rotation", &[0.0, 0.0, 0.0, 1.0])[..] != [v[0], v[1], v[2], t.rotation[3]][..] {
                    return Err(wrong(i, "rotation"));
                }
                if numbers_or(node, "scale", &[1.0; 3])[..] != turned_extent(t.scale, up)[..] {
                    return Err(wrong(i, "scale"));
                }
            }
            Transform::Matrix(m) => {
                // The oracle's own frame change: the matrix applied to each turned basis vector,
                // read back in the turned frame, column by column (glTF's order).
                let got = numbers_or(node, "matrix", &[]);
                let mut want = StdVec::new();
                for col in 0..3 {
                    // The turned frame's basis vector `col`, taken back to the scene's frame.
                    let mut e = [0.0; 3];
                    e[col] = 1.0;
                    let back = unturned(e, up);
                    let image = [0, 1, 2].map(|r| m.linear[r][0] * back[0] + m.linear[r][1] * back[1] + m.linear[r][2] * back[2]);
                    want.extend_from_slice(&turned(image, up));
                    want.push(0.0);
                }
                want.extend_from_slice(&turned(m.translation, up));
                want.push(1.0);
                if got != want {
                    return Err(wrong(i, "matrix"));
                }
                if node.get("translation").is_some() || node.get("rotation").is_some() || node.get("scale").is_some() {
                    return Err(wrong(i, "TRS beside a matrix"));
                }
            }
        }
        if let Some(m) = part.mass
            && extras.get("mass").and_then(|x| x.as_f64()) != Some(m.mass)
        {
            return Err(wrong(i, "extras mass"));
        }
        let levels = list(extras, "lod");
        if levels.len() != part.lods.len() {
            return Err(wrong(i, "extras lod count"));
        }
        for (k, level) in levels.iter().enumerate() {
            let mesh = level.get("mesh").and_then(|x| x.as_u64()).map(at);
            let tris = mesh.and_then(|m| meshes.get(m)).map(|m| triangles_of(g, m));
            let want = part.lods[k].mesh.triangle_count();
            if tris.map(|t| t.len()) != Some(blaze_int::idx(want)) {
                return Err(wrong(i, "extras lod mesh"));
            }
        }
        let Some(view) = part.mesh else {
            if node.get("mesh").is_some() || node_named(nodes, &format!("{}.mesh", part.name)).is_some() {
                return Err(wrong(i, "a mesh for a group"));
            }
            continue;
        };
        let (mesh, dequant) = if quantized {
            let Some(c) = node_named(nodes, &format!("{}.mesh", part.name)) else { return Err(wrong(i, "no .mesh node")) };
            if !children(node).contains(&c) {
                return Err(wrong(i, ".mesh node not the part's child"));
            }
            let child = &nodes[c];
            let offset = numbers_or(child, "translation", &[0.0; 3]);
            let scale = numbers_or(child, "scale", &[1.0; 3]);
            (child.get("mesh").and_then(|x| x.as_u64()), Some((offset, scale)))
        } else {
            (node.get("mesh").and_then(|x| x.as_u64()), None)
        };
        let Some(mesh) = mesh.map(at).and_then(|m| meshes.get(m)) else { return Err(wrong(i, "no mesh")) };
        let Some(position) =
            list(mesh, "primitives").first().and_then(|p| p.get("attributes")).and_then(|a| a.get("POSITION")).and_then(|x| x.as_u64())
        else {
            return Err(wrong(i, "no POSITION"));
        };
        let position = at(position);
        if g.accs[position].count != u64::from(view.vertex_count()) {
            return Err(wrong(i, "vertex count"));
        }
        for (v, &p) in view.positions().iter().enumerate() {
            let want = turned_f32(p, up);
            let raw = g.element(position, v as u64);
            for a in 0..3 {
                let (got, bound) = match &dequant {
                    None => (raw[a], 0.0),
                    Some((offset, scale)) => {
                        // As a GPU computes it: `f32` operations, each rounded.
                        let decoded = raw[a] as f32 * scale[a] as f32 + offset[a] as f32;
                        (f64::from(decoded), quant_bound(offset[a], scale[a]))
                    }
                };
                if (got - f64::from(want[a])).abs() > bound {
                    return Err(Broken::Oracle(OutBroken::PositionWrong {
                        part: i,
                        vertex: v,
                        axis: a,
                        got,
                        want: f64::from(want[a]),
                        bound,
                    }));
                }
            }
        }
        let mut want: StdVec<(Option<u32>, [u32; 3])> =
            view.triangles().iter().zip(view.roles()).map(|(t, r)| (Some(r.0), *t)).collect();
        want.sort_unstable();
        if triangles_of(g, mesh) != want {
            return Err(wrong(i, "triangles or roles"));
        }
        let mut roles: StdVec<u32> = view.roles().iter().map(|r| r.0).collect();
        roles.sort_unstable();
        roles.dedup();
        let names: StdVec<&str> = list(extras, "roles").iter().filter_map(|x| x.as_str()).collect();
        let want_names: StdVec<&str> = roles.iter().map(|&r| scene.materials[blaze_int::idx(r)].name).collect();
        if names != want_names {
            return Err(wrong(i, "extras roles"));
        }
    }
    Ok(())
}

fn children_of_scene(scene: &J) -> StdVec<usize> {
    list(scene, "nodes").iter().filter_map(|c| c.as_u64()).map(at).collect()
}

/// A mesh's triangles with each one's material, sorted.
fn triangles_of(g: &Parsed, mesh: &J) -> StdVec<(Option<u32>, [u32; 3])> {
    let mut out = StdVec::new();
    for prim in list(mesh, "primitives") {
        let material = prim.get("material").and_then(|x| x.as_u64()).and_then(|m| u32::try_from(m).ok());
        let Some(indices) = prim.get("indices").and_then(|x| x.as_u64()).map(at) else { continue };
        for k in 0..g.accs[indices].count / 3 {
            let corner = |c: u64| g.element(indices, 3 * k + c)[0] as u32;
            out.push((material, [corner(0), corner(1), corner(2)]));
        }
    }
    out.sort_unstable();
    out
}

fn glb_case(tape: &Tape, plant: GlbPlant) -> Result<(), Broken<OutBroken>> {
    let drawn = Drawn::draw(tape);
    let encoding = *tape
        .pick(
            &[Encoding::Float, Encoding::Quantized { normals: NormalBits::Eight }, Encoding::Quantized { normals: NormalBits::Sixteen }],
            "encoding",
        )
        .unwrap_or(&Encoding::Float);
    let up = if tape.coin("z up") { Up::Z } else { Up::Y };
    drawn.with_scene(|scene| {
        let opts = GlbOpts { encoding, up, limits: Limits::SHIPPED, alloc: heap(), scratch: heap() };
        let first = glb_planted(scene, opts, plant).map_err(harness)?;
        let parsed = parse(&first).map_err(|why| Broken::Oracle(OutBroken::Invalid { why }))?;
        let second = glb_planted(scene, opts, plant).map_err(harness)?;
        if first[..] != second[..] {
            return Err(Broken::Oracle(OutBroken::NotDeterministic { what: "two exports of one scene" }));
        }
        agrees(&parsed, scene, encoding, up)
    })
}

#[test]
fn glb_exports_validate_and_agree() {
    controls(Config::cases(300), |tape, plant: GlbPlant| glb_case(tape, plant));
}

// ---------------------------------------------------------------------------------------------
// The golden single triangle.

/// The JSON chunk's text, 862 bytes: the asset, one scene of one node, the node's mesh and
/// extras (its name and its one role), one primitive, one material, the three accessors
/// (positions with their bounds, normals, indices) over three views, and the 80-byte buffer.
const GOLDEN_JSON: &[u8] = br#"{"asset":{"generator":"blaze-cad-out 1","version":"2.0"},"scene":0,"scenes":[{"nodes":[0]}],"nodes":[{"name":"tri","mesh":0,"extras":{"name":"tri","roles":["paint.hull"]}}],"meshes":[{"name":"tri","primitives":[{"attributes":{"POSITION":0,"NORMAL":1},"indices":2,"material":0}]}],"materials":[{"name":"paint.hull","pbrMetallicRoughness":{"baseColorFactor":[1,1,1,1],"metallicFactor":0,"roughnessFactor":1}}],"accessors":[{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]},{"bufferView":1,"componentType":5126,"count":3,"type":"VEC3"},{"bufferView":2,"componentType":5123,"count":3,"type":"SCALAR"}],"bufferViews":[{"buffer":0,"byteOffset":0,"byteLength":36,"target":34962},{"buffer":0,"byteOffset":36,"byteLength":36,"target":34962},{"buffer":0,"byteOffset":72,"byteLength":6,"target":34963}],"buffers":[{"byteLength":80}]}"#;

fn golden_bytes() -> StdVec<u8> {
    let mut g = StdVec::new();
    // The header: magic `glTF`, version 2, total length 972 = 12 + (8 + 864) + (8 + 80).
    g.extend_from_slice(b"glTF");
    g.extend_from_slice(&[0x02, 0x00, 0x00, 0x00]);
    g.extend_from_slice(&[0xCC, 0x03, 0x00, 0x00]);
    // The JSON chunk: length 864 = 0x360 (the 862-byte text and two spaces of padding), `JSON`.
    g.extend_from_slice(&[0x60, 0x03, 0x00, 0x00]);
    g.extend_from_slice(b"JSON");
    g.extend_from_slice(GOLDEN_JSON);
    g.extend_from_slice(b"  ");
    // The BIN chunk: length 80 = 0x50, `BIN` and a zero.
    g.extend_from_slice(&[0x50, 0x00, 0x00, 0x00]);
    g.extend_from_slice(b"BIN\0");
    // View 0, bytes 0..36: the positions, little-endian f32 (1.0 is 0x3F800000).
    g.extend_from_slice(&[
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // (0, 0, 0)
        0x00, 0x00, 0x80, 0x3F, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // (1, 0, 0)
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x3F, 0x00, 0x00, 0x00, 0x00, // (0, 1, 0)
    ]);
    // View 1, bytes 36..72: the unit normals, each (0, 0, 1).
    g.extend_from_slice(&[
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x3F, // (0, 0, 1)
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x3F, // (0, 0, 1)
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x3F, // (0, 0, 1)
    ]);
    // View 2, bytes 72..78: the indices 0, 1, 2 as little-endian u16; then two zero bytes of
    // padding to the 80-byte buffer.
    g.extend_from_slice(&[0x00, 0x00, 0x01, 0x00, 0x02, 0x00]);
    g.extend_from_slice(&[0x00, 0x00]);
    g
}

fn golden_case(plant: GlbPlant) -> Result<(), Broken<OutBroken>> {
    if GOLDEN_JSON.len() != 862 {
        return Err(Broken::Harness(format!("the golden text is {} bytes, not 862", GOLDEN_JSON.len())));
    }
    let positions: [P3; 3] = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
    let normals: [P3; 3] = [[0.0, 0.0, 1.0]; 3];
    let view = MeshView::new(Columns {
        positions: &positions,
        normals: &normals,
        tangents: &[],
        uvs: &[],
        colours: &[],
        triangles: &[[0, 1, 2]],
        parts: &[PartId(0)],
        roles: &[MaterialRole(0)],
        creases: &[],
    })
    .map_err(harness)?;
    let parts = [Part {
        name: "tri",
        parent: None,
        transform: Transform::IDENTITY,
        mesh: Some(view),
        lods: &[],
        mass: None,
        joint: None,
        collider: None,
    }];
    let materials = [Material {
        name: "paint.hull",
        base_colour: [1.0; 4],
        metallic: 0.0,
        roughness: 1.0,
        emissive: [0.0; 3],
        emissive_strength: None,
        clearcoat: None,
        double_sided: false,
    }];
    let scene = Scene { parts: &parts, materials: &materials };
    let opts = GlbOpts { encoding: Encoding::Float, up: Up::Y, limits: Limits::SHIPPED, alloc: heap(), scratch: heap() };
    let got = glb_planted(&scene, opts, plant).map_err(harness)?;
    parse(&got).map_err(|why| Broken::Oracle(OutBroken::Invalid { why }))?;
    let want = golden_bytes();
    if let Some(i) = (0..got.len().max(want.len())).find(|&i| got.get(i) != want.get(i)) {
        return Err(Broken::Oracle(OutBroken::GoldenDiffers { at: i, got: got.get(i).copied(), want: want.get(i).copied() }));
    }
    Ok(())
}

/// One fixed scene, so a plain test with no tape: blaze-sim refuses, rightly, a property test
/// whose cases draw nothing. The clean writer gives the golden bytes, and each planted bug is
/// named by the validating parser before the bytes are even compared (the same three plants are
/// also controls of the drawn `glb_exports_validate_and_agree`).
#[test]
fn golden_single_triangle() {
    let clean = golden_case(GlbPlant::None);
    assert!(clean.is_ok(), "the golden triangle: {clean:?}");
    for plant in [GlbPlant::MisalignView, GlbPlant::AccessorPastView, GlbPlant::WrongMinMax] {
        let got = golden_case(plant);
        let named = match (plant, &got) {
            (GlbPlant::MisalignView, Err(Broken::Oracle(OutBroken::Invalid { why: Invalid::MisalignedView { .. } })))
            | (GlbPlant::AccessorPastView, Err(Broken::Oracle(OutBroken::Invalid { why: Invalid::AccessorPastView { .. } })))
            | (GlbPlant::WrongMinMax, Err(Broken::Oracle(OutBroken::Invalid { why: Invalid::MinMax { .. } }))) => true,
            _ => false,
        };
        assert!(named, "the planted {plant:?} was not named by the parser on the golden triangle: {got:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// PNG.

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// `IEND`'s whole chunk: no data, and the CRC-32 of the four bytes `IEND`, 0xAE426082, a
/// constant of the format every PNG ends in.
const IEND: [u8; 12] = [0x00, 0x00, 0x00, 0x00, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82];

struct Decoded {
    width: u32,
    height: u32,
    colour: u8,
    pixels: StdVec<u8>,
}

fn be32(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

/// The Paeth predictor as the specification writes it (section 9.4).
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = i16::from(a) + i16::from(b) - i16::from(c);
    let (pa, pb, pc) = ((p - i16::from(a)).abs(), (p - i16::from(b)).abs(), (p - i16::from(c)).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// A PNG decoder for what the encoder writes: every chunk's CRC checked over its type and data.
fn decode_png_oracle(file: &[u8]) -> Result<Decoded, OutBroken> {
    let bad = |why| OutBroken::PngInvalid { why };
    if file.len() < 8 || file[..8] != SIGNATURE {
        return Err(bad("no signature"));
    }
    let mut i = 8;
    let mut header = None;
    let mut idat = StdVec::new();
    let mut ended = false;
    while i < file.len() {
        if ended {
            return Err(bad("bytes after IEND"));
        }
        if file.len() - i < 12 {
            return Err(bad("a cut chunk"));
        }
        let len = blaze_int::idx(be32(file, i));
        if file.len() - i - 12 < len {
            return Err(bad("a chunk past the file"));
        }
        let kind = [file[i + 4], file[i + 5], file[i + 6], file[i + 7]];
        let data = &file[i + 8..i + 8 + len];
        let mut crc = Crc32::new();
        crc.update(&kind);
        crc.update(data);
        if crc.finish() != be32(file, i + 8 + len) {
            return Err(OutBroken::CrcMismatch { chunk: kind });
        }
        match &kind {
            b"IHDR" => {
                if header.is_some() || i != 8 || len != 13 {
                    return Err(bad("IHDR not first, twice, or not 13 bytes"));
                }
                header = Some((be32(data, 0), be32(data, 4), data[8], data[9], data[10], data[11], data[12]));
            }
            b"IDAT" => {
                if header.is_none() {
                    return Err(bad("IDAT before IHDR"));
                }
                idat.extend_from_slice(data);
            }
            b"IEND" => {
                if len != 0 {
                    return Err(bad("IEND with data"));
                }
                ended = true;
            }
            _ => return Err(bad("a chunk the encoder never writes")),
        }
        i += 12 + len;
    }
    if !ended {
        return Err(bad("no IEND"));
    }
    let (width, height, depth, colour, compression, filter, interlace) = header.ok_or(bad("no IHDR"))?;
    if depth != 8 || compression != 0 || filter != 0 || interlace != 0 || width == 0 || height == 0 {
        return Err(bad("IHDR fields"));
    }
    let bpp = match colour {
        0 => 1,
        6 => 4,
        _ => return Err(bad("colour type")),
    };
    let stride = blaze_int::idx(width) * bpp;
    let rows = blaze_int::idx(height);
    let raw_len = (stride + 1) * rows;
    let mut raw = blaze_own::Vec::new_in(heap());
    let limits = blaze_deflate::Limits { most_out: raw_len as u64, most_ratio: MAX_RATIO, most_header: 0 };
    unzlib(&idat, limits, heap(), &mut raw).map_err(|_| bad("IDAT is no zlib stream of the image's size"))?;
    if raw.len() != raw_len {
        return Err(bad("IDAT inflates to the wrong length"));
    }
    let mut pixels = vec![0u8; stride * rows];
    for y in 0..rows {
        let kind = raw[y * (stride + 1)];
        for x in 0..stride {
            let a = if x >= bpp { pixels[y * stride + x - bpp] } else { 0 };
            let b = if y > 0 { pixels[(y - 1) * stride + x] } else { 0 };
            let c = if y > 0 && x >= bpp { pixels[(y - 1) * stride + x - bpp] } else { 0 };
            let predicted = match kind {
                0 => 0,
                1 => a,
                2 => b,
                3 => ((u16::from(a) + u16::from(b)) / 2) as u8,
                4 => paeth(a, b, c),
                _ => return Err(bad("filter type")),
            };
            pixels[y * stride + x] = raw[y * (stride + 1) + 1 + x].wrapping_add(predicted);
        }
    }
    Ok(Decoded { width, height, colour, pixels })
}

/// A 2×2 grey image whose second row the encoder filters with Paeth only when Paeth's tie is
/// broken toward the upper-left (pixel (1, 1): left 6, up 0, upper-left 4; `p = 2` is 2 from
/// both up and upper-left): the planted tie makes Paeth that row's cheapest filter and its
/// residual decodes, by the specification's predictor, to a different pixel.
const PAETH_TIE: [u8; 4] = [4, 0, 6, 12];

fn png_case(tape: &Tape, plant: PngPlant) -> Result<(), Broken<OutBroken>> {
    let (width, height, kind, pixels) = if tape.one_in(3, "the Paeth tie image") {
        (2, 2, PngKind::Grey8, PAETH_TIE.to_vec())
    } else {
        let kind = if tape.coin("rgba") { PngKind::Rgba8 } else { PngKind::Grey8 };
        let width = tape.int_in(1..=17u32, "width");
        let height = tape.int_in(1..=9u32, "height");
        let small = tape.coin("few values");
        let len = blaze_int::idx(width * height * kind.channels());
        let pixels = (0..len)
            .map(|_| if small { *tape.pick(&[0u8, 2, 4, 6, 8, 10, 12], "value").unwrap_or(&0) } else { tape.int_in(0..=255u8, "value") })
            .collect();
        (width, height, kind, pixels)
    };
    let level = *tape.pick(&[Level::Store, Level::Fast, Level::Default], "level").unwrap_or(&Level::Default);
    let image = Image { width, height, kind, pixels: &pixels };
    let opts = PngOpts { level, limits: Limits::SHIPPED, alloc: heap(), scratch: heap() };
    let file = png_planted(image, opts, plant).map_err(harness)?;
    let again = png_planted(image, opts, plant).map_err(harness)?;
    if file[..] != again[..] {
        return Err(Broken::Oracle(OutBroken::NotDeterministic { what: "two encodings of one image" }));
    }
    let decoded = decode_png_oracle(&file).map_err(Broken::Oracle)?;
    if !file.ends_with(&IEND) {
        return Err(Broken::Oracle(OutBroken::CrcMismatch { chunk: *b"IEND" }));
    }
    let colour = match kind {
        PngKind::Grey8 => 0,
        PngKind::Rgba8 => 6,
    };
    if (decoded.width, decoded.height, decoded.colour) != (width, height, colour) {
        return Err(Broken::Oracle(OutBroken::PngInvalid { why: "IHDR says another size or colour type" }));
    }
    if let Some(i) = (0..pixels.len()).find(|&i| decoded.pixels[i] != pixels[i]) {
        return Err(Broken::Oracle(OutBroken::PixelsDiffer { at: i, got: decoded.pixels[i], want: pixels[i] }));
    }
    Ok(())
}

#[test]
fn png_round_trips_with_crcs_checked() {
    controls(Config::cases(300), |tape, plant: PngPlant| png_case(tape, plant));
}

// ---------------------------------------------------------------------------------------------
// The summary.

/// The summary's lines, formatted here with `core::fmt` (the crate writes them with
/// blaze-text): `{:?}` quotes a name, `{}` writes the shortest round-trip float.
fn expected_summary(scene: &Scene<'_>) -> StdVec<String> {
    scene
        .parts
        .iter()
        .map(|p| {
            let parent = p.parent.map_or_else(|| "-".to_string(), |q| format!("{:?}", scene.parts[blaze_int::idx(q)].name));
            let (triangles, vertices) = p.mesh.map_or((0, 0), |m| (m.triangle_count(), m.vertex_count()));
            let mut roles: StdVec<u32> = p.mesh.map(|m| m.roles().iter().map(|r| r.0).collect()).unwrap_or_default();
            roles.sort_unstable();
            roles.dedup();
            let roles: StdVec<&str> = roles.iter().map(|&r| scene.materials[blaze_int::idx(r)].name).collect();
            let mass = p.mass.map_or_else(|| "-".to_string(), |m| format!("{}", m.mass));
            format!(
                "{:?}\tparent={parent}\ttriangles={triangles}\tvertices={vertices}\troles={}\tmass={mass}\tlods={}\n",
                p.name,
                roles.join(","),
                p.lods.len()
            )
        })
        .collect()
}

fn summary_case(tape: &Tape, plant: SummaryPlant) -> Result<(), Broken<OutBroken>> {
    let drawn = Drawn::draw(tape);
    drawn.with_scene(|scene| {
        let text = summary_planted(scene, SummaryOpts { alloc: heap(), scratch: heap() }, plant).map_err(harness)?;
        let text = core::str::from_utf8(&text).map_err(harness)?;
        let got: StdVec<String> = text.split_inclusive('\n').map(String::from).collect();
        let want = expected_summary(scene);
        for line in 0..got.len().max(want.len()) {
            if got.get(line) != want.get(line) {
                return Err(Broken::Oracle(OutBroken::SummaryWrong {
                    line,
                    got: got.get(line).cloned().unwrap_or_default(),
                    want: want.get(line).cloned().unwrap_or_default(),
                }));
            }
        }
        Ok(())
    })
}

#[test]
fn summary_has_a_line_per_part() {
    controls(Config::cases(200), |tape, plant: SummaryPlant| summary_case(tape, plant));
}

// ---------------------------------------------------------------------------------------------
// Binary STL.

struct StlFacet {
    verts: [[f32; 3]; 3],
}

struct ParsedStl {
    header: [u8; 80],
    facets: StdVec<StlFacet>,
}

fn read_f32_le(bytes: &[u8], i: usize) -> f32 {
    f32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]])
}

/// Independent validating binary STL parser.
fn parse_stl(bytes: &[u8]) -> Result<ParsedStl, StlInvalid> {
    if bytes.len() < 84 {
        return Err(StlInvalid::TooShort { len: bytes.len() });
    }
    let mut header = [0u8; 80];
    header.copy_from_slice(&bytes[0..80]);
    if header[0..6].eq_ignore_ascii_case(b"solid ") {
        return Err(StlInvalid::AsciiHeader);
    }
    let count = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]);
    let want_len = 84usize + blaze_int::idx(count) * 50;
    if bytes.len() != want_len {
        return Err(StlInvalid::LengthMismatch { declared: count, actual_bytes: bytes.len() });
    }
    let mut facets = StdVec::with_capacity(blaze_int::idx(count));
    for f in 0..blaze_int::idx(count) {
        let base = 84 + f * 50;
        let mut raw = [0.0f32; 12];
        for slot in 0..12 {
            let x = read_f32_le(bytes, base + slot * 4);
            if !x.is_finite() {
                return Err(StlInvalid::NonFinite { facet: f, slot });
            }
            if x.to_bits() == 0x8000_0000 {
                return Err(StlInvalid::NegativeZero { facet: f, slot });
            }
            raw[slot] = x;
        }
        let attr = u16::from_le_bytes([bytes[base + 48], bytes[base + 49]]);
        if attr != 0 {
            return Err(StlInvalid::NonZeroAttr { facet: f, attr });
        }
        let normal = [raw[0], raw[1], raw[2]];
        let verts = [[raw[3], raw[4], raw[5]], [raw[6], raw[7], raw[8]], [raw[9], raw[10], raw[11]]];
        let v0 = [f64::from(verts[0][0]), f64::from(verts[0][1]), f64::from(verts[0][2])];
        let v1 = [f64::from(verts[1][0]), f64::from(verts[1][1]), f64::from(verts[1][2])];
        let v2 = [f64::from(verts[2][0]), f64::from(verts[2][1]), f64::from(verts[2][2])];
        let e1 = [v1[0] - v0[0], v1[1] - v0[1], v1[2] - v0[2]];
        let e2 = [v2[0] - v0[0], v2[1] - v0[1], v2[2] - v0[2]];
        let cx = e1[1] * e2[2] - e1[2] * e2[1];
        let cy = e1[2] * e2[0] - e1[0] * e2[2];
        let cz = e1[0] * e2[1] - e1[1] * e2[0];
        let clen = (cx * cx + cy * cy + cz * cz).sqrt();
        if clen > 0.0 {
            let nx = f64::from(normal[0]);
            let ny = f64::from(normal[1]);
            let nz = f64::from(normal[2]);
            let nlen = (nx * nx + ny * ny + nz * nz).sqrt();
            let dot = (nx * cx + ny * cy + nz * cz) / clen;
            if (nlen - 1.0).abs() > 1e-4 || dot < 0.9999 {
                return Err(StlInvalid::NormalDisagrees { facet: f, dot });
            }
        }
        facets.push(StlFacet { verts });
    }
    Ok(ParsedStl { header, facets })
}

struct SolidMesh {
    positions: StdVec<P3>,
    normals: StdVec<P3>,
    triangles: StdVec<[u32; 3]>,
    parts: StdVec<PartId>,
    roles: StdVec<MaterialRole>,
}

impl SolidMesh {
    fn from_verts_tris(positions: StdVec<P3>, triangles: StdVec<[u32; 3]>) -> SolidMesh {
        let normals = vec![[0.0, 0.0, 1.0]; positions.len()];
        let parts = vec![PartId(0); triangles.len()];
        let roles = vec![MaterialRole(0); triangles.len()];
        SolidMesh { positions, normals, triangles, parts, roles }
    }

    fn box_solid(lo: P3, hi: P3) -> SolidMesh {
        let positions = vec![
            [lo[0], lo[1], lo[2]], // 0
            [hi[0], lo[1], lo[2]], // 1
            [hi[0], hi[1], lo[2]], // 2
            [lo[0], hi[1], lo[2]], // 3
            [lo[0], lo[1], hi[2]], // 4
            [hi[0], lo[1], hi[2]], // 5
            [hi[0], hi[1], hi[2]], // 6
            [lo[0], hi[1], hi[2]], // 7
        ];
        let triangles = vec![
            [0, 2, 1], [0, 3, 2], // -Z
            [4, 5, 6], [4, 6, 7], // +Z
            [0, 1, 5], [0, 5, 4], // -Y
            [2, 3, 7], [2, 7, 6], // +Y
            [0, 4, 7], [0, 7, 3], // -X
            [1, 2, 6], [1, 6, 5], // +X
        ];
        Self::from_verts_tris(positions, triangles)
    }

    fn tet_solid(origin: P3, scale: P3) -> SolidMesh {
        let positions = vec![
            [origin[0], origin[1], origin[2]],
            [origin[0] + scale[0], origin[1], origin[2]],
            [origin[0], origin[1] + scale[1], origin[2]],
            [origin[0], origin[1], origin[2] + scale[2]],
        ];
        let triangles = vec![[0, 2, 1], [0, 1, 3], [0, 3, 2], [1, 2, 3]];
        Self::from_verts_tris(positions, triangles)
    }

    fn octahedron_solid(centre: P3, radii: P3) -> SolidMesh {
        let [cx, cy, cz] = centre;
        let [rx, ry, rz] = radii;
        let positions = vec![
            [cx + rx, cy, cz], // 0: +X
            [cx - rx, cy, cz], // 1: -X
            [cx, cy + ry, cz], // 2: +Y
            [cx, cy - ry, cz], // 3: -Y
            [cx, cy, cz + rz], // 4: +Z
            [cx, cy, cz - rz], // 5: -Z
        ];
        let triangles = vec![
            [0, 2, 4], [2, 1, 4], [1, 3, 4], [3, 0, 4],
            [2, 0, 5], [1, 2, 5], [3, 1, 5], [0, 3, 5],
        ];
        Self::from_verts_tris(positions, triangles)
    }

    fn view(&self) -> Result<MeshView<'_>, Broken<OutBroken>> {
        MeshView::new(Columns {
            positions: &self.positions,
            normals: &self.normals,
            tangents: &[],
            uvs: &[],
            colours: &[],
            triangles: &self.triangles,
            parts: &self.parts,
            roles: &self.roles,
            creases: &[],
        })
        .map_err(harness)
    }
}

fn draw_solid(tape: &Tape) -> SolidMesh {
    let pick_pos = |t: &Tape, label: &'static str| -> f32 { f32::from(t.int_in(1i16..=64, label)) / 16.0 };
    let pick_off = |t: &Tape, label: &'static str| -> f32 { f32::from(t.int_in(-32i16..=32, label)) / 16.0 };
    match tape.below(3, "solid kind") {
        0 => {
            let lo = [0.0, pick_off(tape, "lo_y"), pick_off(tape, "lo_z")];
            let hi = [lo[0] + pick_pos(tape, "dx"), lo[1] + pick_pos(tape, "dy"), lo[2] + pick_pos(tape, "dz")];
            SolidMesh::box_solid(lo, hi)
        }
        1 => {
            let origin = [0.0, pick_off(tape, "ty"), pick_off(tape, "tz")];
            let scale = [pick_pos(tape, "sx"), pick_pos(tape, "sy"), pick_pos(tape, "sz")];
            SolidMesh::tet_solid(origin, scale)
        }
        _ => {
            let centre = [0.0, pick_off(tape, "cy"), pick_off(tape, "cz")];
            let radii = [pick_pos(tape, "rx"), pick_pos(tape, "ry"), pick_pos(tape, "rz")];
            SolidMesh::octahedron_solid(centre, radii)
        }
    }
}

fn stl_case(tape: &Tape, plant: StlPlant) -> Result<(), Broken<OutBroken>> {
    let solid = draw_solid(tape);
    let view = solid.view()?;
    let verified = check(view, CheckOpts { limits: MeshLimits::SHIPPED, scratch: heap() }).map_err(harness)?;
    let unit = if tape.coin("metres") { StlUnit::Metres } else { StlUnit::Millimetres };
    let title = *tape
        .pick(
            &[
                "",
                "bracket",
                "solid decoy title",
                "a very long title exceeding eighty bytes to verify clean deterministic truncation at the fixed STL header boundary",
                "bad\tchars\n\x01here",
            ],
            "title",
        )
        .unwrap_or(&"");
    let opts = StlOpts { title, unit, limits: Limits::SHIPPED, alloc: heap() };
    let first = stl_planted(&verified, opts, plant).map_err(harness)?;
    let second = stl_planted(&verified, opts, plant).map_err(harness)?;
    if first[..] != second[..] {
        return Err(Broken::Oracle(OutBroken::NotDeterministic { what: "two STL exports of one mesh" }));
    }
    let parsed = parse_stl(&first).map_err(|why| Broken::Oracle(OutBroken::StlInvalid { why }))?;
    if !parsed.header.starts_with(b"blaze-cad binary stl") {
        return Err(Broken::Oracle(OutBroken::StlInvalid { why: StlInvalid::AsciiHeader }));
    }
    let scale = unit.scale();
    let mut signed_vol6 = 0.0f64;
    for (t, facet) in parsed.facets.iter().enumerate() {
        let want_corners = view.corners(TriId(u32::try_from(t).unwrap_or(0)));
        for k in 0..3 {
            let want = [
                (f64::from(want_corners[k][0]) * scale) as f32,
                (f64::from(want_corners[k][1]) * scale) as f32,
                (f64::from(want_corners[k][2]) * scale) as f32,
            ];
            let got = facet.verts[k];
            for a in 0..3 {
                let w = if want[a] == 0.0 { 0.0 } else { want[a] };
                if got[a] != w {
                    return Err(Broken::Oracle(OutBroken::StlInvalid { why: StlInvalid::WindingMismatch { facet: t } }));
                }
            }
        }
        let v0 = [f64::from(facet.verts[0][0]), f64::from(facet.verts[0][1]), f64::from(facet.verts[0][2])];
        let v1 = [f64::from(facet.verts[1][0]), f64::from(facet.verts[1][1]), f64::from(facet.verts[1][2])];
        let v2 = [f64::from(facet.verts[2][0]), f64::from(facet.verts[2][1]), f64::from(facet.verts[2][2])];
        let cross = [
            v1[1] * v2[2] - v1[2] * v2[1],
            v1[2] * v2[0] - v1[0] * v2[2],
            v1[0] * v2[1] - v1[1] * v2[0],
        ];
        signed_vol6 += v0[0] * cross[0] + v0[1] * cross[1] + v0[2] * cross[2];
    }
    if !(signed_vol6 > 0.0) {
        return Err(Broken::Oracle(OutBroken::StlInvalid { why: StlInvalid::VolumeWrong { volume: signed_vol6 / 6.0 } }));
    }
    Ok(())
}

#[test]
fn stl_exports_validate_and_agree() {
    controls(Config::cases(200), |tape, plant: StlPlant| stl_case(tape, plant));
}

#[test]
fn golden_stl_tetrahedron_and_errors() {
    let solid = SolidMesh::tet_solid([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
    let view = solid.view().expect("tet view");
    let verified = check(view, CheckOpts { limits: MeshLimits::SHIPPED, scratch: heap() }).expect("verified tet");

    let got = stl(
        &verified,
        StlOpts { title: "tet", unit: StlUnit::Metres, limits: Limits::SHIPPED, alloc: heap() },
    )
    .expect("golden stl");

    // Expected 284 bytes = 80 header + 4 count + 4 * 50 facets.
    let mut want = StdVec::with_capacity(284);
    let mut hdr = [0u8; 80];
    let prefix = b"blaze-cad binary stl (m): tet";
    hdr[..prefix.len()].copy_from_slice(prefix);
    want.extend_from_slice(&hdr);
    want.extend_from_slice(&4u32.to_le_bytes());

    let push_facet = |buf: &mut StdVec<u8>, n: [f32; 3], v: [[f32; 3]; 3]| {
        for x in n {
            buf.extend_from_slice(&x.to_le_bytes());
        }
        for p in v {
            for x in p {
                buf.extend_from_slice(&x.to_le_bytes());
            }
        }
        buf.extend_from_slice(&0u16.to_le_bytes());
    };
    let inv_sqrt3 = (1.0f64 / 3.0f64.sqrt()) as f32;
    push_facet(&mut want, [0.0, 0.0, -1.0], [[0.0, 0.0, 0.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]]);
    push_facet(&mut want, [0.0, -1.0, 0.0], [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]]);
    push_facet(&mut want, [-1.0, 0.0, 0.0], [[0.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]]);
    push_facet(&mut want, [inv_sqrt3; 3], [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);

    assert_eq!(got.len(), 284, "golden binary STL length");
    assert_eq!(&got[..], &want[..], "golden binary STL bytes differ");

    // Limits::most_stl_bytes refusal.
    let mut tight = Limits::SHIPPED;
    tight.most_stl_bytes = 200;
    assert_eq!(
        stl(&verified, StlOpts { title: "", unit: StlUnit::Millimetres, limits: tight, alloc: heap() }),
        Err(StlError::TooLarge { bytes: 284, most: 200 }),
    );

    // Millimetres overflow refusal on huge f32 coordinate.
    let huge = SolidMesh::tet_solid([0.0, 0.0, 0.0], [1.0e37, 1.0e37, 1.0e37]);
    let huge_v = check(huge.view().expect("huge view"), CheckOpts { limits: MeshLimits::SHIPPED, scratch: heap() })
        .expect("verified huge");
    assert_eq!(
        stl(&huge_v, StlOpts { title: "", unit: StlUnit::Millimetres, limits: Limits::SHIPPED, alloc: heap() }),
        Err(StlError::Overflow { triangle: 0, corner: 1 }),
    );
}

// ---------------------------------------------------------------------------------------------
// 2D Vector SVG Cross-Section.

struct Subpath {
    pts: StdVec<[f64; 2]>,
    closed: bool,
}

struct ParsedSvg {
    view_box: [f64; 4],
    has_grid: bool,
    has_dimensions: bool,
    has_scale_bar: bool,
    silhouette_even_odd: bool,
    silhouette: StdVec<Subpath>,
    open_contours: StdVec<Subpath>,
}

/// Check well-formed balanced XML tags (independent of the SVG writer).
fn check_xml_balanced(doc: &str) -> Result<(), SvgInvalid> {
    let bad = |why| SvgInvalid::Structure { why };
    if !doc.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>") {
        return Err(bad("missing XML declaration"));
    }
    let bytes = doc.as_bytes();
    let mut i = 0usize;
    let mut stack: StdVec<String> = StdVec::new();
    let mut seen_root = false;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let Some(rel_end) = bytes[i..].iter().position(|&b| b == b'>') else {
            return Err(bad("unterminated tag"));
        };
        let tag = &doc[i + 1..i + rel_end];
        i += rel_end + 1;
        if tag.starts_with('?') {
            if !tag.ends_with('?') {
                return Err(bad("malformed processing instruction"));
            }
            continue;
        }
        if let Some(rest) = tag.strip_prefix('/') {
            let name = rest.trim();
            match stack.pop() {
                Some(open) if open == name => {}
                _ => return Err(bad("mismatched closing tag")),
            }
        } else {
            let self_closing = tag.ends_with('/');
            let body = if self_closing { &tag[..tag.len() - 1] } else { tag };
            let name = body.split_ascii_whitespace().next().unwrap_or("");
            if name.is_empty() {
                return Err(bad("empty tag name"));
            }
            // Check balanced quotes inside attribute list.
            if body.bytes().filter(|&b| b == b'"').count() % 2 != 0 {
                return Err(bad("unbalanced attribute quotes"));
            }
            if stack.is_empty() {
                if seen_root || name != "svg" {
                    return Err(bad("root element must be a single <svg>"));
                }
                seen_root = true;
            }
            if !self_closing {
                stack.push(name.to_string());
            }
        }
    }
    if !seen_root || !stack.is_empty() {
        return Err(bad("unclosed XML tags"));
    }
    Ok(())
}

fn attr_of<'a>(tag: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!(" {key}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')?;
    Some(&tag[start..start + end])
}

fn parse_path_d(d: &str) -> Result<StdVec<Subpath>, SvgInvalid> {
    let bad = |why| SvgInvalid::Structure { why };
    let tokens: StdVec<&str> = d.split_ascii_whitespace().collect();
    let mut subpaths = StdVec::new();
    let mut cur: Option<Subpath> = None;
    let mut i = 0usize;
    while i < tokens.len() {
        match tokens[i] {
            "M" => {
                if let Some(sp) = cur.take() {
                    subpaths.push(sp);
                }
                if i + 2 >= tokens.len() {
                    return Err(bad("truncated M command"));
                }
                let x: f64 = tokens[i + 1].parse().map_err(|_| bad("bad M x"))?;
                let y: f64 = tokens[i + 2].parse().map_err(|_| bad("bad M y"))?;
                cur = Some(Subpath { pts: vec![[x, y]], closed: false });
                i += 3;
            }
            "L" => {
                if i + 2 >= tokens.len() {
                    return Err(bad("truncated L command"));
                }
                let x: f64 = tokens[i + 1].parse().map_err(|_| bad("bad L x"))?;
                let y: f64 = tokens[i + 2].parse().map_err(|_| bad("bad L y"))?;
                let sp = cur.as_mut().ok_or(bad("L without M"))?;
                sp.pts.push([x, y]);
                i += 3;
            }
            "Z" => {
                let sp = cur.as_mut().ok_or(bad("Z without M"))?;
                sp.closed = true;
                subpaths.push(cur.take().unwrap());
                i += 1;
            }
            _ => return Err(bad("unexpected path command")),
        }
    }
    if let Some(sp) = cur {
        subpaths.push(sp);
    }
    Ok(subpaths)
}

fn parse_svg(bytes: &[u8]) -> Result<ParsedSvg, SvgInvalid> {
    let bad = |why| SvgInvalid::Structure { why };
    let doc = core::str::from_utf8(bytes).map_err(|_| bad("not UTF-8"))?;
    check_xml_balanced(doc)?;

    let mut view_box = None;
    let mut has_grid = false;
    let mut has_dimensions = false;
    let mut has_scale_bar = false;
    let mut silhouette_even_odd = true;
    let mut silhouette = StdVec::new();
    let mut open_contours = StdVec::new();

    for part in doc.split('<').skip(1) {
        let Some(end) = part.find('>') else { continue };
        let tag = &part[..end];
        if tag.starts_with("svg ") {
            if attr_of(tag, "xmlns") != Some("http://www.w3.org/2000/svg") || attr_of(tag, "version") != Some("1.1") {
                return Err(bad("missing SVG 1.1 xmlns or version"));
            }
            let vb_str = attr_of(tag, "viewBox").ok_or(bad("missing viewBox"))?;
            let nums: StdVec<f64> = vb_str
                .split_ascii_whitespace()
                .map(|s| s.parse::<f64>())
                .collect::<Result<_, _>>()
                .map_err(|_| bad("bad viewBox numbers"))?;
            if nums.len() != 4 || !nums.iter().all(|x| x.is_finite()) || !(nums[2] > 0.0 && nums[3] > 0.0) {
                return Err(bad("invalid viewBox"));
            }
            view_box = Some([nums[0], nums[1], nums[2], nums[3]]);
        } else if tag.starts_with("g ") {
            match attr_of(tag, "id") {
                Some("grid") => has_grid = true,
                Some("dimensions") => has_dimensions = true,
                Some("scale-bar") => has_scale_bar = true,
                _ => {}
            }
        } else if tag.starts_with("path ") {
            match attr_of(tag, "id") {
                Some("silhouette") => {
                    silhouette_even_odd = attr_of(tag, "fill-rule") == Some("evenodd");
                    let d = attr_of(tag, "d").ok_or(bad("silhouette missing d"))?;
                    silhouette = parse_path_d(d)?;
                }
                Some("open-contours") => {
                    let d = attr_of(tag, "d").ok_or(bad("open-contours missing d"))?;
                    open_contours = parse_path_d(d)?;
                }
                _ => {}
            }
        }
    }
    let view_box = view_box.ok_or(bad("missing <svg>"))?;
    Ok(ParsedSvg { view_box, has_grid, has_dimensions, has_scale_bar, silhouette_even_odd, silhouette, open_contours })
}

/// Convert SVG Y-down subpath points back to math Y-up (mm) and compute perimeter & signed area.
fn loop_metrics_math_mm(sp: &Subpath) -> (f64, f64, [f64; 2]) {
    let n = sp.pts.len();
    let mut perim = 0.0f64;
    let mut area2 = 0.0f64;
    let mut sx = 0.0f64;
    let mut sy = 0.0f64;
    for i in 0..n {
        let a = [sp.pts[i][0], -sp.pts[i][1]];
        let b = [sp.pts[(i + 1) % n][0], -sp.pts[(i + 1) % n][1]];
        let dx = b[0] - a[0];
        let dy = b[1] - a[1];
        perim += (dx * dx + dy * dy).sqrt();
        area2 += a[0] * b[1] - b[0] * a[1];
        sx += a[0];
        sy += a[1];
    }
    (perim, 0.5 * area2, [sx / (n as f64), sy / (n as f64)])
}

fn sample_grid(nx: u32, ny: u32, origin: [f64; 2], step: f64, f: impl Fn(f64, f64) -> f64) -> StdVec<f64> {
    let mut values = StdVec::with_capacity(blaze_int::idx(nx) * blaze_int::idx(ny));
    for iy in 0..ny {
        let y = origin[1] + f64::from(iy) * step;
        for ix in 0..nx {
            let x = origin[0] + f64::from(ix) * step;
            values.push(f(x, y));
        }
    }
    values
}

enum SvgScenario {
    Circle { cx: f64, cy: f64, r: f64, step: f64 },
    Annulus { cx: f64, cy: f64, r_in: f64, r_out: f64, step: f64 },
    Saddle { connected: bool, cy_m: f64 },
}

fn svg_case(tape: &Tape, plant: SvgPlant) -> Result<(), Broken<OutBroken>> {
    let kind = tape.below(3, "svg scenario");
    let show_grid = tape.coin("show_grid");
    let show_dimensions = tape.coin("show_dimensions");

    let (nx, ny, origin, step, values, scenario) = match kind {
        0 => {
            // Circle centred strictly above Y=0 so NoYFlip is unambiguously detected.
            let r = f64::from(tape.int_in(14i32..=28, "radius_mm")) * 0.001;
            let cx = f64::from(tape.int_in(-10i32..=10, "cx_mm")) * 0.001;
            let cy = f64::from(tape.int_in(20i32..=40, "cy_mm")) * 0.001;
            let step = 0.0015;
            let half = r + 4.0 * step;
            let nx = ((2.0 * half / step) as u32).max(28) + 2;
            let ny = nx;
            let origin = [cx - half, cy - half];
            let values = sample_grid(nx, ny, origin, step, |x, y| ((x - cx).powi(2) + (y - cy).powi(2)).sqrt() - r);
            (nx, ny, origin, step, values, SvgScenario::Circle { cx, cy, r, step })
        }
        1 => {
            // Annulus with interior hole: 2 closed loops (CCW outer + CW hole).
            let r_in = f64::from(tape.int_in(10i32..=14, "r_in_mm")) * 0.001;
            let r_out = r_in + f64::from(tape.int_in(10i32..=16, "wall_mm")) * 0.001;
            let cx = f64::from(tape.int_in(-8i32..=8, "cx_mm")) * 0.001;
            let cy = f64::from(tape.int_in(22i32..=38, "cy_mm")) * 0.001;
            let step = 0.0015;
            let half = r_out + 4.0 * step;
            let nx = ((2.0 * half / step) as u32).max(36) + 2;
            let ny = nx;
            let origin = [cx - half, cy - half];
            let values = sample_grid(nx, ny, origin, step, |x, y| {
                let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
                (r_in - d).max(d - r_out)
            });
            (nx, ny, origin, step, values, SvgScenario::Annulus { cx, cy, r_in, r_out, step })
        }
        _ => {
            // Explicit 4x4 grid with central saddle cell (1,1)-(2,2) surrounded by +5 outside.
            let case10 = tape.coin("saddle_case_10");
            let connected = tape.coin("saddle_connected");
            let (in_val, out_val) = if connected { (-2.0, 0.5) } else { (-0.5, 2.0) };
            let nx = 4u32;
            let ny = 4u32;
            let step = 0.005;
            let origin = [0.0, 0.02]; // strictly positive Y (20..35 mm)
            let mut values = vec![5.0f64; 16];
            let set = |vals: &mut [f64], x: usize, y: usize, v: f64| vals[y * 4 + x] = v;
            if case10 {
                // BR (2,1) & TL (1,2) inside; BL (1,1) & TR (2,2) outside.
                set(&mut values, 1, 1, out_val);
                set(&mut values, 2, 1, in_val);
                set(&mut values, 2, 2, out_val);
                set(&mut values, 1, 2, in_val);
            } else {
                // Case 5: BL (1,1) & TR (2,2) inside; BR (2,1) & TL (1,2) outside.
                set(&mut values, 1, 1, in_val);
                set(&mut values, 2, 1, out_val);
                set(&mut values, 2, 2, in_val);
                set(&mut values, 1, 2, out_val);
            }
            let cy_m = origin[1] + 1.5 * step;
            (nx, ny, origin, step, values, SvgScenario::Saddle { connected, cy_m })
        }
    };

    let grid = SectionGrid { nx, ny, origin, step, values: &values };
    let opts = SvgOpts {
        title: "Section <A&B>",
        fill: DEFAULT_FILL,
        stroke: DEFAULT_STROKE,
        stroke_width: 0.0,
        show_dimensions,
        show_grid,
        limits: Limits::SHIPPED,
        alloc: heap(),
        scratch: heap(),
    };
    let first = svg_planted(&grid, opts, plant).map_err(harness)?;
    let second = svg_planted(&grid, opts, plant).map_err(harness)?;
    if first[..] != second[..] {
        return Err(Broken::Oracle(OutBroken::NotDeterministic { what: "two SVG exports of one grid" }));
    }
    let parsed = parse_svg(&first).map_err(|why| Broken::Oracle(OutBroken::SvgInvalid { why }))?;
    if !parsed.has_scale_bar || parsed.has_grid != show_grid || parsed.has_dimensions != show_dimensions {
        return Err(Broken::Oracle(OutBroken::SvgInvalid {
            why: SvgInvalid::Structure { why: "missing expected drawing layer" },
        }));
    }
    if !parsed.silhouette_even_odd {
        return Err(Broken::Oracle(OutBroken::SvgInvalid { why: SvgInvalid::MissingEvenOdd }));
    }
    if parsed.silhouette.iter().any(|sp| !sp.closed) {
        return Err(Broken::Oracle(OutBroken::SvgInvalid { why: SvgInvalid::UnclosedSilhouette }));
    }
    if !parsed.open_contours.is_empty() {
        return Err(Broken::Oracle(OutBroken::SvgInvalid {
            why: SvgInvalid::Structure { why: "unexpected open contours on closed shape" },
        }));
    }

    match scenario {
        SvgScenario::Circle { cx, cy, r, step } => {
            if parsed.silhouette.len() != 1 {
                return Err(Broken::Oracle(OutBroken::SvgInvalid {
                    why: SvgInvalid::WrongLoopCount { got: parsed.silhouette.len(), want: 1 },
                }));
            }
            let sp = &parsed.silhouette[0];
            let (perim, area, centroid) = loop_metrics_math_mm(sp);
            let want_cy = cy * 1000.0;
            if (centroid[1] - want_cy).abs() > 0.5 * want_cy.abs() {
                return Err(Broken::Oracle(OutBroken::SvgInvalid {
                    why: SvgInvalid::WrongYSign { got_cy: centroid[1], want_cy },
                }));
            }
            let r_mm = r * 1000.0;
            let cx_mm = cx * 1000.0;
            let step_mm = step * 1000.0;
            // Every contour vertex must sit tightly on the zero level set (midpoint staircase fails badly).
            for &[x, y_svg] in &sp.pts {
                let d = ((x - cx_mm).powi(2) + (-y_svg - want_cy).powi(2)).sqrt();
                if (d - r_mm).abs() > 0.2 * step_mm {
                    return Err(Broken::Oracle(OutBroken::SvgInvalid {
                        why: SvgInvalid::GeometryError { why: "vertex off circle zero-crossing", got: d, want: r_mm },
                    }));
                }
            }
            let want_perim = 2.0 * core::f64::consts::PI * r_mm;
            let want_area = core::f64::consts::PI * r_mm * r_mm;
            if (perim - want_perim).abs() / want_perim > 0.02 {
                return Err(Broken::Oracle(OutBroken::SvgInvalid {
                    why: SvgInvalid::GeometryError { why: "circle perimeter", got: perim, want: want_perim },
                }));
            }
            if !(area > 0.0) || (area - want_area).abs() / want_area > 0.02 {
                return Err(Broken::Oracle(OutBroken::SvgInvalid {
                    why: SvgInvalid::GeometryError { why: "circle signed area", got: area, want: want_area },
                }));
            }
        }
        SvgScenario::Annulus { cx, cy, r_in, r_out, step } => {
            if parsed.silhouette.len() != 2 {
                return Err(Broken::Oracle(OutBroken::SvgInvalid {
                    why: SvgInvalid::WrongLoopCount { got: parsed.silhouette.len(), want: 2 },
                }));
            }
            let want_cy = cy * 1000.0;
            let mut pos_area = 0.0;
            let mut neg_area = 0.0;
            let cx_mm = cx * 1000.0;
            let r_in_mm = r_in * 1000.0;
            let r_out_mm = r_out * 1000.0;
            let step_mm = step * 1000.0;
            for sp in &parsed.silhouette {
                let (_perim, area, centroid) = loop_metrics_math_mm(sp);
                if (centroid[1] - want_cy).abs() > 0.5 * want_cy.abs() {
                    return Err(Broken::Oracle(OutBroken::SvgInvalid {
                        why: SvgInvalid::WrongYSign { got_cy: centroid[1], want_cy },
                    }));
                }
                let target_r = if area > 0.0 {
                    pos_area += area;
                    r_out_mm
                } else {
                    neg_area += -area;
                    r_in_mm
                };
                for &[x, y_svg] in &sp.pts {
                    let d = ((x - cx_mm).powi(2) + (-y_svg - want_cy).powi(2)).sqrt();
                    if (d - target_r).abs() > 0.25 * step_mm {
                        return Err(Broken::Oracle(OutBroken::SvgInvalid {
                            why: SvgInvalid::GeometryError { why: "annulus vertex off level set", got: d, want: target_r },
                        }));
                    }
                }
            }
            let want_out = core::f64::consts::PI * r_out_mm * r_out_mm;
            let want_in = core::f64::consts::PI * r_in_mm * r_in_mm;
            if (pos_area - want_out).abs() / want_out > 0.025 {
                return Err(Broken::Oracle(OutBroken::SvgInvalid {
                    why: SvgInvalid::GeometryError { why: "annulus outer area", got: pos_area, want: want_out },
                }));
            }
            if (neg_area - want_in).abs() / want_in > 0.035 {
                return Err(Broken::Oracle(OutBroken::SvgInvalid {
                    why: SvgInvalid::GeometryError { why: "annulus hole area", got: neg_area, want: want_in },
                }));
            }
        }
        SvgScenario::Saddle { connected, cy_m } => {
            let want_loops = if connected { 1 } else { 2 };
            if parsed.silhouette.len() != want_loops {
                return Err(Broken::Oracle(OutBroken::SvgInvalid {
                    why: SvgInvalid::WrongLoopCount { got: parsed.silhouette.len(), want: want_loops },
                }));
            }
            let want_cy = cy_m * 1000.0;
            for sp in &parsed.silhouette {
                let (_perim, area, centroid) = loop_metrics_math_mm(sp);
                if (centroid[1] - want_cy).abs() > 0.5 * want_cy.abs() {
                    return Err(Broken::Oracle(OutBroken::SvgInvalid {
                        why: SvgInvalid::WrongYSign { got_cy: centroid[1], want_cy },
                    }));
                }
                if !(area > 0.0) {
                    return Err(Broken::Oracle(OutBroken::SvgInvalid {
                        why: SvgInvalid::GeometryError { why: "saddle loop orientation", got: area, want: 1.0 },
                    }));
                }
            }
        }
    }
    Ok(())
}

#[test]
fn svg_sections_validate_and_agree() {
    controls(Config::cases(200), |tape, plant: SvgPlant| svg_case(tape, plant));
}

#[test]
fn svg_edge_cases_open_empty_zero_and_errors() {
    let base_opts = |limits: Limits| SvgOpts {
        title: "Cut <1> & \"2\"",
        fill: DEFAULT_FILL,
        stroke: DEFAULT_STROKE,
        stroke_width: 0.25,
        show_dimensions: true,
        show_grid: true,
        limits,
        alloc: heap(),
        scratch: heap(),
    };

    // 1. Open contour: half-plane x <= 0.005 slicing through the grid boundary.
    let vals_open = sample_grid(10, 10, [0.0, 0.0], 0.002, |x, _y| x - 0.005);
    let open_svg = svg(&SectionGrid { nx: 10, ny: 10, origin: [0.0, 0.0], step: 0.002, values: &vals_open }, base_opts(Limits::SHIPPED))
        .expect("open svg");
    let parsed_open = parse_svg(&open_svg).expect("parse open svg");
    assert!(parsed_open.silhouette.is_empty(), "open half-plane has no closed loops");
    assert_eq!(parsed_open.open_contours.len(), 1, "expected one open contour polyline");
    assert!(!parsed_open.open_contours[0].closed, "open contour must not close with Z");
    let open_str = core::str::from_utf8(&open_svg).expect("utf8");
    assert!(open_str.contains("<title>Cut &lt;1&gt; &amp; &quot;2&quot;</title>"), "XML title escaping");

    // 2. Empty contour (all positive samples) still produces valid standalone SVG with viewBox.
    let vals_empty = vec![1.0f64; 16];
    let empty_svg = svg(&SectionGrid { nx: 4, ny: 4, origin: [0.0, 0.0], step: 0.01, values: &vals_empty }, base_opts(Limits::SHIPPED))
        .expect("empty svg");
    let parsed_empty = parse_svg(&empty_svg).expect("parse empty svg");
    assert!(parsed_empty.silhouette.is_empty());
    assert!(parsed_empty.open_contours.is_empty());
    assert!(parsed_empty.view_box[2] > 0.0 && parsed_empty.view_box[3] > 0.0);

    // 3. Exact zeros on grid vertices: power-of-two step (1/128 m = 7.8125 mm) so every boundary
    //    sample evaluates to bit-exact 0.0 without FP drift.
    let h = 1.0 / 128.0;
    let vals_zero = sample_grid(9, 9, [-4.0 * h, -4.0 * h], h, |x, y| x.abs().max(y.abs()) - 2.0 * h);
    assert!(vals_zero.contains(&0.0), "grid must hit bit-exact 0.0 on boundary nodes");
    let zero_svg = svg(&SectionGrid { nx: 9, ny: 9, origin: [-4.0 * h, -4.0 * h], step: h, values: &vals_zero }, base_opts(Limits::SHIPPED))
        .expect("exact zero svg");
    let parsed_zero = parse_svg(&zero_svg).expect("parse exact zero svg");
    assert_eq!(parsed_zero.silhouette.len(), 1);
    let (_p, area, _c) = loop_metrics_math_mm(&parsed_zero.silhouette[0]);
    // 4h × 4h square minus four corner right triangles of legs h (area 0.5 h^2 each) = 14 h^2.
    let h_mm = h * 1000.0;
    let want_area = 14.0 * h_mm * h_mm;
    assert!((area - want_area).abs() < 1e-6, "exact-zero square contour area should be {want_area}, got {area}");

    // 4. Error refusals.
    assert_eq!(
        svg(&SectionGrid { nx: 1, ny: 4, origin: [0.0, 0.0], step: 0.01, values: &[0.0; 4] }, base_opts(Limits::SHIPPED)),
        Err(SvgError::EmptyGrid { nx: 1, ny: 4 }),
    );
    let mut tiny = Limits::SHIPPED;
    tiny.most_svg_cells = 4;
    assert_eq!(
        svg(&SectionGrid { nx: 4, ny: 4, origin: [0.0, 0.0], step: 0.01, values: &vals_empty }, base_opts(tiny)),
        Err(SvgError::TooManyCells { cells: 9, most: 4 }),
    );
    assert_eq!(
        svg(&SectionGrid { nx: 4, ny: 4, origin: [0.0, 0.0], step: 0.01, values: &[0.0; 10] }, base_opts(Limits::SHIPPED)),
        Err(SvgError::Length { want: 16, got: 10 }),
    );
    assert_eq!(
        svg(&SectionGrid { nx: 4, ny: 4, origin: [0.0, 0.0], step: 0.0, values: &vals_empty }, base_opts(Limits::SHIPPED)),
        Err(SvgError::BadGrid),
    );
    let mut nan_vals = vec![1.0f64; 16];
    nan_vals[7] = f64::NAN;
    assert_eq!(
        svg(&SectionGrid { nx: 4, ny: 4, origin: [0.0, 0.0], step: 0.01, values: &nan_vals }, base_opts(Limits::SHIPPED)),
        Err(SvgError::NonFinite { index: 7 }),
    );
    let mut bad_style = base_opts(Limits::SHIPPED);
    bad_style.stroke_width = -1.0;
    assert_eq!(
        svg(&SectionGrid { nx: 4, ny: 4, origin: [0.0, 0.0], step: 0.01, values: &vals_empty }, bad_style),
        Err(SvgError::BadStyle),
    );
}

// ---------------------------------------------------------------------------------------------
// PNG decoder & textured GLB export / read_glb round-trip.

/// Build a hand-crafted PNG with an explicit filter byte per row (supports colour types 0, 2, 6)
/// so `decode_png` is guaranteed to exercise every filter type 0..=4 and RGB8 expansion.
fn craft_png_with_filters(width: u32, height: u32, colour_type: u8, row_filters: &[u8], pixels: &[u8]) -> StdVec<u8> {
    let bpp: usize = match colour_type {
        0 => 1,
        2 => 3,
        6 => 4,
        _ => panic!("unsupported test colour_type"),
    };
    let stride = blaze_int::idx(width) * bpp;
    let rows = blaze_int::idx(height);
    assert_eq!(row_filters.len(), rows);
    assert_eq!(pixels.len(), stride * rows);

    let mut raw = StdVec::with_capacity((stride + 1) * rows);
    for y in 0..rows {
        let f = row_filters[y];
        raw.push(f);
        for x in 0..stride {
            let cur = pixels[y * stride + x];
            let a = if x >= bpp { pixels[y * stride + x - bpp] } else { 0 };
            let b = if y > 0 { pixels[(y - 1) * stride + x] } else { 0 };
            let c = if y > 0 && x >= bpp { pixels[(y - 1) * stride + x - bpp] } else { 0 };
            let pred = match f {
                0 => 0,
                1 => a,
                2 => b,
                3 => ((u16::from(a) + u16::from(b)) / 2) as u8,
                4 => paeth(a, b, c),
                _ => panic!("bad test filter"),
            };
            raw.push(cur.wrapping_sub(pred));
        }
    }
    let mut zdata = blaze_own::Vec::new_in(heap());
    blaze_deflate::zlib(&raw, Level::Default, heap(), &mut zdata).expect("zlib");

    let mut out = StdVec::new();
    out.extend_from_slice(&SIGNATURE);
    let push_chunk = |buf: &mut StdVec<u8>, kind: &[u8; 4], data: &[u8]| {
        buf.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        buf.extend_from_slice(kind);
        buf.extend_from_slice(data);
        let mut crc = Crc32::new();
        crc.update(kind);
        crc.update(data);
        buf.extend_from_slice(&crc.finish().to_be_bytes());
    };
    let mut ihdr = [0u8; 13];
    ihdr[0..4].copy_from_slice(&width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&height.to_be_bytes());
    ihdr[8] = 8;
    ihdr[9] = colour_type;
    push_chunk(&mut out, b"IHDR", &ihdr);
    push_chunk(&mut out, b"IDAT", &zdata);
    push_chunk(&mut out, b"IEND", &[]);
    out
}

#[test]
fn png_round_trips_rgba8_and_grey8_all_filters() {
    // 1. Round-trip RGBA8 and Grey8 images encoded by `png()` across multiple patterns.
    let (w, h) = (13u32, 10u32);
    let mut rgba = StdVec::with_capacity(blaze_int::idx(w * h * 4));
    let mut grey = StdVec::with_capacity(blaze_int::idx(w * h));
    for y in 0..h {
        for x in 0..w {
            let r = ((x * 19 + y * 37) & 0xFF) as u8;
            let g = ((x * 53 ^ y * 17) & 0xFF) as u8;
            let b = ((x.wrapping_mul(x) + y * 11) & 0xFF) as u8;
            let a = if (x + y) % 3 == 0 { 255 } else { ((x * 23 + y * 7) & 0xFF) as u8 };
            rgba.extend_from_slice(&[r, g, b, a]);
            grey.push(r.wrapping_add(g));
        }
    }

    for (kind, pixels) in [(PngKind::Rgba8, &rgba[..]), (PngKind::Grey8, &grey[..])] {
        for level in [Level::Store, Level::Fast, Level::Default] {
            let encoded = png(
                Image { width: w, height: h, kind, pixels },
                PngOpts { level, limits: Limits::SHIPPED, alloc: heap(), scratch: heap() },
            )
            .expect("png encode");
            let decoded = crate::decode_png(&encoded, Limits::SHIPPED, heap(), heap()).expect("png decode");
            assert_eq!(decoded.width, w);
            assert_eq!(decoded.height, h);
            assert_eq!(decoded.kind, kind);
            assert_eq!(&decoded.pixels[..], pixels);
        }
    }

    // 2. Explicitly exercise every filter type 0..=4 for Grey8, RGBA8, and RGB8 (expanded to RGBA8).
    let filters = [0u8, 1, 2, 3, 4, 4, 3, 2, 1, 0];
    let grey_all = craft_png_with_filters(w, h, 0, &filters, &grey);
    let dec_grey = crate::decode_png(&grey_all, Limits::SHIPPED, heap(), heap()).expect("decode grey all filters");
    assert_eq!(dec_grey.kind, PngKind::Grey8);
    assert_eq!(&dec_grey.pixels[..], &grey[..]);

    let rgba_all = craft_png_with_filters(w, h, 6, &filters, &rgba);
    let dec_rgba = crate::decode_png(&rgba_all, Limits::SHIPPED, heap(), heap()).expect("decode rgba all filters");
    assert_eq!(dec_rgba.kind, PngKind::Rgba8);
    assert_eq!(&dec_rgba.pixels[..], &rgba[..]);

    let mut rgb = StdVec::with_capacity(blaze_int::idx(w * h * 3));
    let mut rgb_as_rgba = StdVec::with_capacity(blaze_int::idx(w * h * 4));
    for chunk in rgba.chunks_exact(4) {
        rgb.extend_from_slice(&chunk[0..3]);
        rgb_as_rgba.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
    }
    let rgb_all = craft_png_with_filters(w, h, 2, &filters, &rgb);
    let dec_rgb = crate::decode_png(&rgb_all, Limits::SHIPPED, heap(), heap()).expect("decode rgb8 all filters");
    assert_eq!(dec_rgb.kind, PngKind::Rgba8);
    assert_eq!(&dec_rgb.pixels[..], &rgb_as_rgba[..]);

    // 3. Reject corrupted signature, corrupted CRC, truncated payload, trailing bytes, and pixel cap.
    let mut bad_sig = rgba_all.clone();
    bad_sig[0] ^= 0xFF;
    assert_eq!(crate::decode_png(&bad_sig, Limits::SHIPPED, heap(), heap()), Err(PngError::BadSignature));

    let mut bad_crc = rgba_all.clone();
    // Flip a bit in IHDR's CRC (bytes 29..33).
    bad_crc[30] ^= 0x01;
    assert!(matches!(
        crate::decode_png(&bad_crc, Limits::SHIPPED, heap(), heap()),
        Err(PngError::CrcMismatch { chunk, .. }) if chunk == *b"IHDR"
    ));

    let truncated = &rgba_all[..rgba_all.len() - 5];
    assert!(matches!(crate::decode_png(truncated, Limits::SHIPPED, heap(), heap()), Err(PngError::Truncated { .. })));

    let mut trailing = rgba_all.clone();
    trailing.push(0x42);
    assert_eq!(
        crate::decode_png(&trailing, Limits::SHIPPED, heap(), heap()),
        Err(PngError::TrailingBytes { extra: 1 }),
    );

    let mut tight = Limits::SHIPPED;
    tight.most_pixels = 50;
    assert_eq!(
        crate::decode_png(&rgba_all, tight, heap(), heap()),
        Err(PngError::TooManyPixels { pixels: 130, most: 50 }),
    );
}

#[test]
fn glb_textured_writes_uvs_tangents_embedded_pngs_and_round_trips_through_read_glb() {
    // 1. Create 3 deterministic PNG textures: baseColor (RGBA8 4x4), metallicRoughness (RGBA8 2x2), normal (Grey8 2x2 -> expanded).
    let base_pixels: StdVec<u8> = (0u8..64).map(|i| i.wrapping_mul(7).wrapping_add(13)).collect();
    let mr_pixels: StdVec<u8> = vec![0, 128, 64, 255, 0, 200, 32, 255, 0, 96, 192, 255, 0, 160, 240, 255];
    let norm_pixels: StdVec<u8> = vec![128, 128, 255, 255, 140, 118, 250, 255, 118, 140, 250, 255, 128, 128, 255, 255];

    let base_png = png(
        Image { width: 4, height: 4, kind: PngKind::Rgba8, pixels: &base_pixels },
        PngOpts { level: Level::Default, limits: Limits::SHIPPED, alloc: heap(), scratch: heap() },
    )
    .expect("base png");
    let mr_png = png(
        Image { width: 2, height: 2, kind: PngKind::Rgba8, pixels: &mr_pixels },
        PngOpts { level: Level::Default, limits: Limits::SHIPPED, alloc: heap(), scratch: heap() },
    )
    .expect("mr png");
    let norm_png = png(
        Image { width: 2, height: 2, kind: PngKind::Rgba8, pixels: &norm_pixels },
        PngOpts { level: Level::Default, limits: Limits::SHIPPED, alloc: heap(), scratch: heap() },
    )
    .expect("norm png");

    // 2. Multi-material mesh on a non-cubic bounding box (so quantized normal inverse-transpose scaling is exercised!).
    let positions: [P3; 6] = [
        [0.0, 0.0, 0.0],
        [4.0, 0.0, 0.0],
        [4.0, 1.0, 0.0],
        [0.0, 1.0, 0.0],
        [2.0, 0.5, 2.0],
        [2.0, 0.5, -0.5],
    ];
    let normals: [P3; 6] = [
        [0.0, 0.0, 1.0],
        [0.0, 0.6, 0.8],
        [0.6, 0.0, 0.8],
        [0.0, -0.8, 0.6],
        [0.0, 0.0, 1.0],
        [0.0, 0.0, -1.0],
    ];
    let uvs: [[f32; 2]; 6] = [
        [0.0, 0.0],
        [1.0, 0.0],
        [1.0, 1.0],
        [0.0, 1.0],
        [0.5, 0.5],
        [0.25, 0.75],
    ];
    let tangents: [[f32; 4]; 6] = [
        [1.0, 0.0, 0.0, 1.0],
        [1.0, 0.0, 0.0, -1.0],
        [0.0, 1.0, 0.0, 1.0],
        [0.0, 0.0, 1.0, -1.0],
        [0.6, 0.8, 0.0, 1.0],
        [0.0, 0.6, 0.8, -1.0],
    ];
    let triangles: [[u32; 3]; 4] = [[0, 1, 2], [0, 2, 3], [0, 1, 4], [2, 3, 5]];
    let tri_parts = [PartId(0); 4];
    let tri_roles = [MaterialRole(0), MaterialRole(0), MaterialRole(1), MaterialRole(1)];
    let main_view = MeshView::new(Columns {
        positions: &positions,
        normals: &normals,
        tangents: &tangents,
        uvs: &uvs,
        colours: &[],
        triangles: &triangles,
        parts: &tri_parts,
        roles: &tri_roles,
        creases: &[],
    })
    .expect("main view");

    // Child part mesh (1 triangle, role 1, UVs + tangents).
    let child_pos: [P3; 3] = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 2.0, 0.5]];
    let child_norm: [P3; 3] = [[0.0, 0.0, 1.0]; 3];
    let child_uvs: [[f32; 2]; 3] = [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]];
    let child_tan: [[f32; 4]; 3] = [[1.0, 0.0, 0.0, 1.0]; 3];
    let child_view = MeshView::new(Columns {
        positions: &child_pos,
        normals: &child_norm,
        tangents: &child_tan,
        uvs: &child_uvs,
        colours: &[],
        triangles: &[[0, 1, 2]],
        parts: &[PartId(0)],
        roles: &[MaterialRole(1)],
        creases: &[],
    })
    .expect("child view");

    let root_trs = Transform::Trs(Trs {
        translation: [1.5, -2.0, 0.75],
        rotation: [0.0, 0.6, 0.0, 0.8],
        scale: [2.0, 1.0, 0.5],
    });
    let child_mat = Transform::Matrix(Matrix {
        linear: [[0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]],
        translation: [0.25, 0.5, -0.75],
    });
    let root_mass = MassFacts {
        mass: 12.5,
        centre: [1.0, 0.5, 0.25],
        inertia: [[2.0, 0.25, -0.5], [0.25, 3.0, 0.125], [-0.5, 0.125, 4.0]],
    };
    let lods = [Lod {
        mesh: child_view,
        error: blaze_units::Length::mm(5.0),
        switch_distance: blaze_units::Length::m(15.0),
    }];

    let parts = [
        Part {
            name: "hull",
            parent: None,
            transform: root_trs,
            mesh: Some(main_view),
            lods: &lods,
            mass: Some(root_mass),
            joint: None,
            collider: None,
        },
        Part {
            name: "mount",
            parent: Some(0),
            transform: child_mat,
            mesh: Some(child_view),
            lods: &[],
            mass: None,
            joint: None,
            collider: None,
        },
        Part {
            name: "sensor_group",
            parent: Some(1),
            transform: Transform::IDENTITY,
            mesh: None,
            lods: &[],
            mass: None,
            joint: None,
            collider: None,
        },
    ];
    let mats = [
        Material {
            name: "painted_steel",
            base_colour: [0.8, 0.2, 0.1, 1.0],
            metallic: 0.75,
            roughness: 0.375,
            emissive: [0.0; 3],
            emissive_strength: None,
            clearcoat: Some(Clearcoat { factor: 0.5, roughness: 0.125 }),
            double_sided: false,
        },
        Material {
            name: "beacon_glass",
            base_colour: [0.25, 0.75, 1.0, 0.5],
            metallic: 0.0,
            roughness: 0.125,
            emissive: [0.5, 0.25, 0.125],
            emissive_strength: Some(3.0),
            clearcoat: None,
            double_sided: true,
        },
    ];
    let mat_textures = [
        MaterialTextures {
            base_colour: Some(0),
            metallic_roughness: Some(1),
            normal: Some(2),
            normal_scale: 0.75,
        },
        MaterialTextures {
            base_colour: Some(0),
            metallic_roughness: None,
            normal: Some(2),
            normal_scale: 1.0,
        },
    ];
    let png_slices: [&[u8]; 3] = [&base_png[..], &mr_png[..], &norm_png[..]];
    let scene = Scene { parts: &parts, materials: &mats };
    let tex_pack = TexturePack { png_images: &png_slices, material_textures: &mat_textures };

    for encoding in [
        Encoding::Float,
        Encoding::Quantized { normals: NormalBits::Sixteen },
        Encoding::Quantized { normals: NormalBits::Eight },
    ] {
        for up in [Up::Z, Up::Y] {
            let opts = GlbOpts { encoding, up, limits: Limits::SHIPPED, alloc: heap(), scratch: heap() };
            let first = glb_textured(&scene, &tex_pack, opts).expect("glb_textured");
            let second = glb_textured(&scene, &tex_pack, opts).expect("glb_textured deterministic");
            assert_eq!(&first[..], &second[..], "textured GLB must be byte-deterministic");

            // Validate against independent glTF 2.0 parser.
            parse(&first).expect("validating glTF parser");

            // Read back through `read_glb` in the same `up` frame.
            let loaded = read_glb(
                &first,
                GlbReadOpts { up, limits: Limits::SHIPPED, alloc: heap(), scratch: heap() },
            )
            .expect("read_glb");

            // Hierarchy & transforms.
            assert_eq!(loaded.parts.len(), 3);
            assert_eq!(loaded.parts[0].name.as_str(), "hull");
            assert_eq!(loaded.parts[0].parent, None);
            assert_eq!(loaded.parts[0].transform, root_trs);
            assert_eq!(loaded.parts[0].mass, Some(root_mass));
            assert_eq!(loaded.parts[0].lods.len(), 1);
            assert_eq!(loaded.parts[0].lods[0].error, blaze_units::Length::mm(5.0));
            assert_eq!(loaded.parts[0].lods[0].switch_distance, blaze_units::Length::m(15.0));

            assert_eq!(loaded.parts[1].name.as_str(), "mount");
            assert_eq!(loaded.parts[1].parent, Some(0));
            assert_eq!(loaded.parts[1].transform, child_mat);

            assert_eq!(loaded.parts[2].name.as_str(), "sensor_group");
            assert_eq!(loaded.parts[2].parent, Some(1));
            assert!(loaded.parts[2].mesh.is_none());

            // Verify composed world_matrix on child part matches parent * child.
            let want_world = root_trs.to_matrix().compose(child_mat.to_matrix());
            assert_eq!(loaded.world_matrix(1), want_world);

            // Materials & texture bindings.
            assert_eq!(loaded.materials.len(), 2);
            assert_eq!(loaded.materials[0].as_material(), mats[0]);
            assert_eq!(loaded.materials[0].textures, mat_textures[0]);
            assert_eq!(loaded.materials[1].as_material(), mats[1]);
            assert_eq!(loaded.materials[1].textures, mat_textures[1]);

            // Decoded embedded PNG images.
            assert_eq!(loaded.images.len(), 3);
            assert_eq!((loaded.images[0].width, loaded.images[0].height), (4, 4));
            assert_eq!(&loaded.images[0].rgba[..], &base_pixels[..]);
            assert_eq!((loaded.images[1].width, loaded.images[1].height), (2, 2));
            assert_eq!(&loaded.images[1].rgba[..], &mr_pixels[..]);
            assert_eq!((loaded.images[2].width, loaded.images[2].height), (2, 2));
            assert_eq!(&loaded.images[2].rgba[..], &norm_pixels[..]);

            // Main mesh positions, normals, UVs, tangents, triangles & material roles.
            let mesh = loaded.parts[0].mesh.as_ref().expect("hull mesh");
            let mv = mesh.view();
            assert_eq!(mv.vertex_count(), 6);
            assert_eq!(mv.triangle_count(), 4);
            let pos_tol = match encoding {
                Encoding::Float => 0.0f64,
                Encoding::Quantized { .. } => 2e-4,
            };
            let norm_tol = match encoding {
                Encoding::Float => 1e-6f64,
                Encoding::Quantized { normals: NormalBits::Sixteen } => 2e-3,
                Encoding::Quantized { normals: NormalBits::Eight } => 4e-2,
            };
            for (v, (&got_p, &want_p)) in mv.positions().iter().zip(positions.iter()).enumerate() {
                for a in 0..3 {
                    let diff = (f64::from(got_p[a]) - f64::from(want_p[a])).abs();
                    assert!(diff <= pos_tol, "pos[{v}][{a}] mismatch ({encoding:?}, {up:?}): got {} want {}", got_p[a], want_p[a]);
                }
            }
            for (v, (&got_n, &want_n)) in mv.normals().iter().zip(normals.iter()).enumerate() {
                for a in 0..3 {
                    let diff = (f64::from(got_n[a]) - f64::from(want_n[a])).abs();
                    assert!(diff <= norm_tol, "norm[{v}][{a}] mismatch ({encoding:?}, {up:?}): got {} want {}", got_n[a], want_n[a]);
                }
            }
            let got_uvs = mv.uvs().expect("TEXCOORD_0 round-tripped");
            assert_eq!(got_uvs, &uvs[..]);
            let got_tangents = mv.tangents().expect("TANGENT round-tripped");
            for (v, (&got_t, &want_t)) in got_tangents.iter().zip(tangents.iter()).enumerate() {
                for a in 0..3 {
                    assert!(
                        (f64::from(got_t[a]) - f64::from(want_t[a])).abs() <= 1e-6,
                        "tangent[{v}][{a}] mismatch: got {} want {}",
                        got_t[a],
                        want_t[a]
                    );
                }
                assert_eq!(got_t[3], want_t[3], "tangent handedness w must match");
            }
            let mut got_tris: StdVec<(MaterialRole, [u32; 3])> =
                mv.roles().iter().copied().zip(mv.triangles().iter().copied()).collect();
            let mut want_tris: StdVec<(MaterialRole, [u32; 3])> =
                tri_roles.iter().copied().zip(triangles.iter().copied()).collect();
            got_tris.sort_unstable();
            want_tris.sort_unstable();
            assert_eq!(got_tris, want_tris);
        }
    }

    // 3. Error refusals on invalid textures / UVs / tangents / GLB container.
    let bad_img: [&[u8]; 1] = [b"not a png"];
    let bad_pack = TexturePack { png_images: &bad_img, material_textures: &[] };
    assert_eq!(
        glb_textured(
            &scene,
            &bad_pack,
            GlbOpts { encoding: Encoding::Float, up: Up::Y, limits: Limits::SHIPPED, alloc: heap(), scratch: heap() }
        ),
        Err(GlbError::InvalidPngImage { image: 0 }),
    );
    let oob_mt = [MaterialTextures { base_colour: Some(9), ..MaterialTextures::NONE }];
    assert_eq!(
        glb_textured(
            &scene,
            &TexturePack { png_images: &png_slices, material_textures: &oob_mt },
            GlbOpts { encoding: Encoding::Float, up: Up::Y, limits: Limits::SHIPPED, alloc: heap(), scratch: heap() }
        ),
        Err(GlbError::UnknownTexture { material: 0, index: 9, count: 3 }),
    );
    let neg_scale_mt = [MaterialTextures { normal: Some(2), normal_scale: -0.5, ..MaterialTextures::NONE }];
    assert_eq!(
        glb_textured(
            &scene,
            &TexturePack { png_images: &png_slices, material_textures: &neg_scale_mt },
            GlbOpts { encoding: Encoding::Float, up: Up::Y, limits: Limits::SHIPPED, alloc: heap(), scratch: heap() }
        ),
        Err(GlbError::BadMaterial { material: 0, field: MaterialField::NormalScale }),
    );
    let too_many_mt = [MaterialTextures::NONE; 3];
    assert_eq!(
        glb_textured(
            &scene,
            &TexturePack { png_images: &png_slices, material_textures: &too_many_mt },
            GlbOpts { encoding: Encoding::Float, up: Up::Y, limits: Limits::SHIPPED, alloc: heap(), scratch: heap() }
        ),
        Err(GlbError::TooManyMaterialTextures { got: 3, materials: 2 }),
    );
    assert_eq!(
        read_glb(b"short", GlbReadOpts { up: Up::Y, limits: Limits::SHIPPED, alloc: heap(), scratch: heap() }).unwrap_err(),
        GlbReadError::TooShort { len: 5 },
    );
    assert_eq!(
        read_glb(
            b"NOPE\x02\x00\x00\x00\x14\x00\x00\x00\x00\x00\x00\x00JSON",
            GlbReadOpts { up: Up::Y, limits: Limits::SHIPPED, alloc: heap(), scratch: heap() }
        )
        .unwrap_err(),
        GlbReadError::BadMagic,
    );
}
