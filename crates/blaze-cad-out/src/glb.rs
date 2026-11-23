//! A binary glTF 2.0 writer (Khronos, "glTF 2.0 Specification", 2021, section 4.4 for the GLB
//! container), from first principles.
//!
//! # The container
//!
//! A 12-byte header (`glTF`, version 2, the whole length), then a JSON chunk (its length, `JSON`,
//! the text padded with spaces to a multiple of 4), then, when any mesh is written, a BIN chunk
//! (its length, `BIN\0`, the data). Every length is little-endian `u32`, so a `.glb` is at most
//! `u32::MAX` bytes ([`Limits::most_glb_bytes`]).
//!
//! # The BIN chunk
//!
//! One buffer. Each accessor has its own buffer view, and every view starts on a 4-byte
//! boundary (zeros between), so every element of every vertex attribute is 4-byte aligned as
//! the specification asks (3.6.2.4) and every offset is a multiple of its component's size. The
//! buffer's `byteLength` is the chunk's, a multiple of 4. A part's mesh is one glTF mesh: one
//! `POSITION` and one `NORMAL` accessor shared by one primitive per role, each with its own
//! indices (`u16` when the mesh has at most 65,535 vertices, so 65,535, the restart value glTF
//! forbids, never occurs; `u32` otherwise) and the role's material.
//!
//! # Float and quantized
//!
//! [`Encoding::Float`]: positions and unit normals as `f32` `VEC3`, `POSITION` with its exact
//! `min` and `max`. [`Encoding::Quantized`] (`KHR_mesh_quantization`, in `extensionsRequired`):
//! positions as `u16` `VEC3` (stride 8) on a grid over the PART's box (its mesh, levels of detail
//! and collider hulls together, so they share one transform), with blaze-cad-mesh's stated
//! round-trip bound; normals as normalized `i8` (stride 4) or `i16` (stride 8) `VEC3`. The
//! dequantization `p = offset + q · scale` is the transform of a child node `<name>.mesh` that
//! carries the mesh (so the part's own children are not scaled with it), and is also written in
//! the part's `extras.dequantize` for its levels of detail and hulls, which no node draws. A
//! renderer turns normals by the inverse transpose of that scale, so each stored normal is the
//! unit vector along `scale ⊙ n` (an axis of zero extent gets scale 1: its `q` is 0, so the
//! position does not change, and the normal is turned by nothing there).
//!
//! # A part's node
//!
//! `name`; `children` (its `.mesh` node first, then its child parts in scene order);
//! `translation`, `rotation` and `scale` where not the identity's; `mesh` (float encoding); and
//! `extras`: `name`, `roles` (the material names its mesh uses, in role order), `mass`,
//! `centre`, `inertia` (rows), `joint` (`name`, `kind`, `axis`, `lower`, `upper`), `collider`
//! (`kind`: `box` with `centre` and `half`, `sphere` with `centre` and `radius`, `hulls` with the
//! `meshes` indices, or `mesh` with the part's own), `lod` (each level's `mesh` index, `error`
//! and `switch_distance`; levels of detail are written as EXTRAS, not `MSFT_lod`: blaze's switch
//! is a distance with a stated error, MSFT_lod's is a screen coverage, and a viewer that ignores
//! extras still draws the full mesh), and `dequantize` when quantized. Absent facts are left
//! out, never written as zero.
//!
//! # Materials
//!
//! One per role, in role order: `pbrMetallicRoughness` (`baseColorFactor`, `metallicFactor`,
//! `roughnessFactor`), `emissiveFactor` where not black, `alphaMode: BLEND` where alpha is below
//! 1, `doubleSided` where asked, and in `extensions` `KHR_materials_clearcoat`
//! (`clearcoatFactor`, `clearcoatRoughnessFactor`) and `KHR_materials_emissive_strength`
//! (`emissiveStrength`), each listed in `extensionsUsed` (not required: a viewer without them
//! draws the base material).
//!
//! # Determinism
//!
//! Keys in the fixed order above; arrays in scene, role and triangle order; numbers by the
//! crate's emitter; nothing else read. Two calls on one scene give equal bytes.

#![blaze::sans_io]

use blaze_cad_mesh::geom::{Aabb, P3, narrow, plus_zero, plus_zeros, round_even, unit, wide};
use blaze_cad_mesh::{Columns, Dequantize, MaterialRole, MeshView, QuantError, QuantOpts, ShapeError, Within, quantize_positions};
use blaze_int::{Int, idx};
use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{Alloc, AllocFailure, Vec};
use blaze_text::Put;
use blaze_units::Length;

use crate::Limits;
use crate::json::{Json, JsonFault};
use crate::plant::GlbPlant;
use crate::png::SIGNATURE as PNG_SIGNATURE;
use crate::scene::{
    Collider, Material, MaterialTextures, Matrix, Part, Scene, TexturePack, Transform, Up, turn, turn_extent, turn_rotation,
    turn_tensor,
};
use crate::sink::Bytes;

type V<'a, T> = Vec<T, Alloc<'a>>;

/// The `asset.generator` written.
pub const GENERATOR: &str = "blaze-cad-out 1";

const FLOAT: u64 = 5126;
const BYTE: u64 = 5120;
const SHORT: u64 = 5122;
const UNSIGNED_SHORT: u64 = 5123;
const UNSIGNED_INT: u64 = 5125;
const ARRAY_BUFFER: u64 = 34962;
const ELEMENT_ARRAY_BUFFER: u64 = 34963;
const MAG_LINEAR: u64 = 9729;
const MIN_LINEAR_MIPMAP_LINEAR: u64 = 9987;
const WRAP_REPEAT: u64 = 10497;

/// How many bits each quantized normal component has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NormalBits {
    Eight,
    Sixteen,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Encoding {
    Float,
    /// `KHR_mesh_quantization` (module documentation).
    Quantized { normals: NormalBits },
}

#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct GlbOpts<'a, 's> {
    pub encoding: Encoding,
    pub up: Up,
    pub limits: Limits,
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

/// A part's field that a refusal names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Translation,
    Rotation,
    Scale,
    Mass,
    Centre,
    Inertia,
    JointAxis,
    JointLimits,
    ColliderCentre,
    ColliderSize,
    LodError,
    LodSwitchDistance,
    /// A matrix transform that is not finite or is singular.
    Matrix,
}

/// A material's field that a refusal names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MaterialField {
    BaseColour,
    Metallic,
    Roughness,
    Emissive,
    EmissiveStrength,
    Clearcoat,
    ClearcoatRoughness,
    NormalScale,
}

/// Which of a part's meshes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MeshSlot {
    Main,
    /// Level `k`, from 1 (level 0 is the main mesh).
    Lod(u32),
    /// Collider hull `k`, from 0.
    Hull(u32),
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum GlbError {
    TooManyParts { parts: usize, most: u32 },
    /// A parent that is not an earlier part.
    ParentOrder { part: u32, parent: u32 },
    /// A number that is NaN or infinite, or out of its range (a negative mass, size or
    /// distance, a joint's lower limit above its upper, a zero scale).
    BadValue { part: u32, field: Field },
    /// A rotation more than `2^-20` from unit length.
    NotUnitRotation { part: u32, length: f64 },
    /// A triangle's role past the scene's materials.
    UnknownRole { part: u32, mesh: MeshSlot, role: MaterialRole },
    /// A material factor that is not finite or outside its range.
    BadMaterial { material: u32, field: MaterialField },
    /// More `material_textures` entries than `scene.materials`.
    TooManyMaterialTextures { got: usize, materials: usize },
    /// An embedded image slice does not start with the 8-byte PNG signature.
    InvalidPngImage { image: u32 },
    /// A material texture index past `textures.png_images`.
    UnknownTexture { material: u32, index: u32, count: usize },
    EmptyMesh { part: u32, mesh: MeshSlot },
    /// A vertex normal of no direction.
    ZeroNormal { part: u32, mesh: MeshSlot, vertex: u32 },
    /// A vertex UV coordinate that is not finite.
    BadUv { part: u32, mesh: MeshSlot, vertex: u32 },
    /// A vertex tangent whose `xyz` is zero/non-finite or whose handedness `w` is not `±1`.
    BadTangent { part: u32, mesh: MeshSlot, vertex: u32 },
    Quant { part: u32, mesh: MeshSlot, error: QuantError },
    /// The positions-only view quantization reads was refused (a bug: it is built from a
    /// mesh already shape-checked).
    Shape { part: u32, mesh: MeshSlot, error: ShapeError },
    TooLarge { bytes: u64, most: u64 },
    /// The emitter's fault (a bug in this module).
    Json(JsonFault),
    Alloc(AllocFailure),
}

impl From<AllocFailure> for GlbError {
    fn from(e: AllocFailure) -> GlbError {
        GlbError::Alloc(e)
    }
}

/// `scene` as one `.glb`, in `opts.alloc`.
pub fn glb<'a>(scene: &Scene<'_>, opts: GlbOpts<'a, '_>) -> Result<Vec<u8, Alloc<'a>>, GlbError> {
    glb_textured_planted(scene, &TexturePack::EMPTY, opts, NoPlant)
}

/// `scene` with embedded PNG textures and per-material texture bindings as one `.glb`, in `opts.alloc`.
pub fn glb_textured<'a>(
    scene: &Scene<'_>,
    textures: &TexturePack<'_>,
    opts: GlbOpts<'a, '_>,
) -> Result<Vec<u8, Alloc<'a>>, GlbError> {
    glb_textured_planted(scene, textures, opts, NoPlant)
}

#[derive(Clone, Copy)]
struct ViewRec {
    offset: u32,
    length: u32,
    /// 0 for tightly packed (no `byteStride` written).
    stride: u32,
    target: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AccType {
    Scalar,
    Vec2,
    Vec3,
    Vec4,
}

impl AccType {
    fn name(self) -> &'static str {
        match self {
            AccType::Scalar => "SCALAR",
            AccType::Vec2 => "VEC2",
            AccType::Vec3 => "VEC3",
            AccType::Vec4 => "VEC4",
        }
    }
}

#[derive(Clone, Copy)]
enum Bounds {
    None,
    F32 { min: P3, max: P3 },
    U16 { min: [u16; 3], max: [u16; 3] },
}

#[derive(Clone, Copy)]
struct AccRec {
    view: u32,
    component: u64,
    normalized: bool,
    count: u32,
    kind: AccType,
    bounds: Bounds,
}

#[derive(Clone, Copy)]
struct PrimRec {
    indices: u32,
    material: Option<u32>,
}

#[derive(Clone, Copy)]
struct MeshRec {
    part: u32,
    slot: MeshSlot,
    position: u32,
    normal: u32,
    texcoord: Option<u32>,
    tangent: Option<u32>,
    prims: (u32, u32),
}

#[derive(Clone, Copy)]
struct PartRec {
    main: Option<u32>,
    lods: (u32, u32),
    hulls: (u32, u32),
    /// The dequantization offset and the scale the node carries (zero axes as 1).
    dequant: Option<(P3, P3)>,
}

/// `n` as a 32-bit count, or the file is too large.
fn count32(n: usize) -> Result<u32, GlbError> {
    blaze_int::len32(n).map_err(|_| GlbError::TooLarge { bytes: n.widen_to(), most: u64::from(u32::MAX) })
}

/// The BIN chunk as it is laid out, with the views, accessors, primitives and meshes in it.
struct Bin<'s, P> {
    bytes: V<'s, u8>,
    views: V<'s, ViewRec>,
    accs: V<'s, AccRec>,
    prims: V<'s, PrimRec>,
    meshes: V<'s, MeshRec>,
    scratch: Alloc<'s>,
    plant: P,
    /// Which planted bug has fired (each fires once): misaligned view, accessor past view,
    /// wrong min/max.
    fired: [bool; 3],
}

impl<'s, P: Planted<GlbPlant>> Bin<'s, P> {
    fn put(&mut self, bytes: &[u8]) -> Result<(), GlbError> {
        self.bytes.try_extend_from_slice(bytes)?;
        Ok(())
    }

    fn pad(&mut self) -> Result<(), GlbError> {
        while self.bytes.len() % 4 != 0 {
            self.put(&[0])?;
        }
        Ok(())
    }

    fn begin_view(&mut self) -> Result<u32, GlbError> {
        self.pad()?;
        if self.plant.has(GlbPlant::MisalignView) && !self.fired[0] {
            self.fired[0] = true;
            self.put(&[0])?;
        }
        count32(self.bytes.len())
    }

    fn end_view(&mut self, start: u32, stride: u32, target: Option<u64>) -> Result<u32, GlbError> {
        let end = count32(self.bytes.len())?;
        self.views.try_push(ViewRec { offset: start, length: end - start, stride, target }).map_err(|r| r.why)?;
        self.pad()?;
        count32(self.views.len() - 1)
    }

    fn accessor(&mut self, mut acc: AccRec) -> Result<u32, GlbError> {
        if self.plant.has(GlbPlant::AccessorPastView) && !self.fired[1] {
            self.fired[1] = true;
            acc.count += 1;
        }
        self.accs.try_push(acc).map_err(|r| r.why)?;
        count32(self.accs.len() - 1)
    }

    fn position_bounds(&mut self, bounds: Bounds) -> Bounds {
        if !self.plant.has(GlbPlant::WrongMinMax) || self.fired[2] {
            return bounds;
        }
        self.fired[2] = true;
        match bounds {
            Bounds::F32 { min, mut max } => {
                max[0] = min[0];
                Bounds::F32 { min, max }
            }
            Bounds::U16 { min, mut max } => {
                max[0] = min[0];
                Bounds::U16 { min, max }
            }
            Bounds::None => Bounds::None,
        }
    }
}

/// A position or normal in glTF's frame, `-0` made `+0` (exact: a signed permutation).
fn turn_p3(p: P3, up: Up) -> P3 {
    plus_zeros(match up {
        Up::Y => p,
        Up::Z => [p[0], p[2], -p[1]],
    })
}

/// What [`put_mesh`] needs beyond the mesh.
struct MeshCtx<'m> {
    part: u32,
    slot: MeshSlot,
    up: Up,
    encoding: Encoding,
    /// The part's box in glTF's frame, when quantized.
    within: Option<Aabb>,
    /// The scene's materials, for a mesh drawn with them; `None` for a collider hull.
    materials: Option<&'m [Material<'m>]>,
}

/// `view` laid into the BIN chunk as one glTF mesh; its index and, when quantized, the
/// dequantization.
fn put_mesh<P: Planted<GlbPlant>>(
    bin: &mut Bin<'_, P>,
    view: &MeshView<'_>,
    ctx: &MeshCtx<'_>,
) -> Result<(u32, Option<Dequantize>), GlbError> {
    let (part, slot) = (ctx.part, ctx.slot);
    let s = bin.scratch;
    if view.triangle_count() == 0 {
        return Err(GlbError::EmptyMesh { part, mesh: slot });
    }
    let nv = view.vertex_count();
    let mut positions: V<'_, P3> = Vec::try_with_capacity_in(view.positions().len(), s)?;
    for &p in view.positions() {
        positions.try_push(turn_p3(p, ctx.up)).map_err(|r| r.why)?;
    }
    let mut units: V<'_, [f64; 3]> = Vec::try_with_capacity_in(view.normals().len(), s)?;
    for (v, &n) in view.normals().iter().enumerate() {
        let Some(u) = unit(wide(turn_p3(n, ctx.up))) else {
            return Err(GlbError::ZeroNormal { part, mesh: slot, vertex: count32(v)? });
        };
        units.try_push(u).map_err(|r| r.why)?;
    }
    let (position, normal, dequant) = match ctx.encoding {
        Encoding::Float => {
            let start = bin.begin_view()?;
            let mut b = Aabb::EMPTY;
            for p in positions.iter() {
                b.include(*p);
                for x in p {
                    bin.put(&x.to_le_bytes())?;
                }
            }
            let pv = bin.end_view(start, 0, Some(ARRAY_BUFFER))?;
            let bounds = bin.position_bounds(Bounds::F32 { min: b.min, max: b.max });
            let position = bin.accessor(AccRec {
                view: pv,
                component: FLOAT,
                normalized: false,
                count: nv,
                kind: AccType::Vec3,
                bounds,
            })?;
            let start = bin.begin_view()?;
            for &u in units.iter() {
                for x in plus_zeros(narrow(u)) {
                    bin.put(&x.to_le_bytes())?;
                }
            }
            let nview = bin.end_view(start, 0, Some(ARRAY_BUFFER))?;
            let normal = bin.accessor(AccRec {
                view: nview,
                component: FLOAT,
                normalized: false,
                count: nv,
                kind: AccType::Vec3,
                bounds: Bounds::None,
            })?;
            (position, normal, None)
        }
        Encoding::Quantized { normals } => {
            // Quantization reads positions alone; a view needs a normals column of the same
            // length, and the positions serve.
            let qview = MeshView::new(Columns {
                positions: &positions,
                normals: &positions,
                tangents: &[],
                uvs: &[],
                colours: &[],
                triangles: &[],
                parts: &[],
                roles: &[],
                creases: &[],
            })
            .map_err(|error| GlbError::Shape { part, mesh: slot, error })?;
            let within = match ctx.within {
                Some(b) => Within::Box(b),
                None => Within::OwnBounds,
            };
            let q = quantize_positions(&qview, QuantOpts { within, alloc: s })
                .map_err(|error| GlbError::Quant { part, mesh: slot, error })?;
            let start = bin.begin_view()?;
            let (mut lo, mut hi) = ([u16::MAX; 3], [0u16; 3]);
            for code in q.values.iter() {
                for a in 0..3 {
                    lo[a] = lo[a].min(code[a]);
                    hi[a] = hi[a].max(code[a]);
                    bin.put(&code[a].to_le_bytes())?;
                }
                bin.put(&[0, 0])?;
            }
            let pv = bin.end_view(start, 8, Some(ARRAY_BUFFER))?;
            let bounds = bin.position_bounds(Bounds::U16 { min: lo, max: hi });
            let position = bin.accessor(AccRec {
                view: pv,
                component: UNSIGNED_SHORT,
                normalized: false,
                count: nv,
                kind: AccType::Vec3,
                bounds,
            })?;
            let scale = node_scale(q.dequantize);
            let start = bin.begin_view()?;
            for (v, &u) in units.iter().enumerate() {
                let Some(w) = unit([u[0] * f64::from(scale[0]), u[1] * f64::from(scale[1]), u[2] * f64::from(scale[2])])
                else {
                    return Err(GlbError::ZeroNormal { part, mesh: slot, vertex: count32(v)? });
                };
                match normals {
                    NormalBits::Eight => {
                        let c = w.map(|x| snorm(x, 127.0) as i8);
                        bin.put(&[c[0].to_le_bytes()[0], c[1].to_le_bytes()[0], c[2].to_le_bytes()[0], 0])?;
                    }
                    NormalBits::Sixteen => {
                        for x in w {
                            bin.put(&(snorm(x, 32767.0) as i16).to_le_bytes())?;
                        }
                        bin.put(&[0, 0])?;
                    }
                }
            }
            let (stride, component) = match normals {
                NormalBits::Eight => (4, BYTE),
                NormalBits::Sixteen => (8, SHORT),
            };
            let nview = bin.end_view(start, stride, Some(ARRAY_BUFFER))?;
            let normal = bin.accessor(AccRec {
                view: nview,
                component,
                normalized: true,
                count: nv,
                kind: AccType::Vec3,
                bounds: Bounds::None,
            })?;
            (position, normal, Some(q.dequantize))
        }
    };
    let texcoord = match view.uvs() {
        Some(uvs) => {
            let start = bin.begin_view()?;
            for (v, &[u, w]) in uvs.iter().enumerate() {
                if !u.is_finite() || !w.is_finite() {
                    return Err(GlbError::BadUv { part, mesh: slot, vertex: count32(v)? });
                }
                bin.put(&plus_zero(u).to_le_bytes())?;
                bin.put(&plus_zero(w).to_le_bytes())?;
            }
            let tv = bin.end_view(start, 0, Some(ARRAY_BUFFER))?;
            let acc = bin.accessor(AccRec {
                view: tv,
                component: FLOAT,
                normalized: false,
                count: nv,
                kind: AccType::Vec2,
                bounds: Bounds::None,
            })?;
            Some(acc)
        }
        None => None,
    };
    let tangent = match view.tangents() {
        Some(tangents) => {
            let start = bin.begin_view()?;
            for (v, &t) in tangents.iter().enumerate() {
                let w = t[3];
                if w != 1.0 && w != -1.0 {
                    return Err(GlbError::BadTangent { part, mesh: slot, vertex: count32(v)? });
                }
                let Some(u) = unit(wide(turn_p3([t[0], t[1], t[2]], ctx.up))) else {
                    return Err(GlbError::BadTangent { part, mesh: slot, vertex: count32(v)? });
                };
                for x in plus_zeros(narrow(u)) {
                    bin.put(&x.to_le_bytes())?;
                }
                bin.put(&w.to_le_bytes())?;
            }
            let tv = bin.end_view(start, 0, Some(ARRAY_BUFFER))?;
            let acc = bin.accessor(AccRec {
                view: tv,
                component: FLOAT,
                normalized: false,
                count: nv,
                kind: AccType::Vec4,
                bounds: Bounds::None,
            })?;
            Some(acc)
        }
        None => None,
    };
    // Indices, one primitive per role run (or one for a hull), triangles in input order within it.
    let first_prim = count32(bin.prims.len())?;
    let wide_indices = nv > 65_535;
    let triangles = view.triangles();
    let mut order: V<'_, (u32, u32)> = Vec::try_with_capacity_in(triangles.len(), s)?;
    for (t, role) in view.roles().iter().enumerate() {
        let key = if ctx.materials.is_some() { role.0 } else { 0 };
        order.try_push((key, count32(t)?)).map_err(|r| r.why)?;
    }
    order.sort_unstable();
    let mut at = 0usize;
    while at < order.len() {
        let role = order[at].0;
        let mut end = at;
        while end < order.len() && order[end].0 == role {
            end += 1;
        }
        let material = match ctx.materials {
            Some(materials) => {
                if idx(role) >= materials.len() {
                    return Err(GlbError::UnknownRole { part, mesh: slot, role: MaterialRole(role) });
                }
                Some(role)
            }
            None => None,
        };
        let start = bin.begin_view()?;
        for &(_, t) in &order[at..end] {
            for v in triangles[idx(t)] {
                if wide_indices {
                    bin.put(&v.to_le_bytes())?;
                } else {
                    // At most 65,535 vertices: every index fits in 16 bits.
                    bin.put(&v.narrow_or_stop::<u16>().to_le_bytes())?;
                }
            }
        }
        let iview = bin.end_view(start, 0, Some(ELEMENT_ARRAY_BUFFER))?;
        let indices = bin.accessor(AccRec {
            view: iview,
            component: if wide_indices { UNSIGNED_INT } else { UNSIGNED_SHORT },
            normalized: false,
            count: count32((end - at) * 3)?,
            kind: AccType::Scalar,
            bounds: Bounds::None,
        })?;
        bin.prims.try_push(PrimRec { indices, material }).map_err(|r| r.why)?;
        at = end;
    }
    let prims = (first_prim, count32(bin.prims.len())?);
    bin.meshes.try_push(MeshRec { part, slot, position, normal, texcoord, tangent, prims }).map_err(|r| r.why)?;
    Ok((count32(bin.meshes.len() - 1)?, dequant))
}

/// The node's scale: each axis's dequantization scale, 1 for an axis of zero extent.
fn node_scale(d: Dequantize) -> P3 {
    d.scale.map(|x| if x == 0.0 { 1.0 } else { x })
}

/// A signed normalized integer's value for `x` in `[-1, 1]` (glTF 3.11: `c = round(x · m)`).
fn snorm(x: f64, m: f64) -> f64 {
    round_even(x.clamp(-1.0, 1.0) * m)
}

fn finite3(v: [f64; 3]) -> bool {
    v.iter().all(|x| x.is_finite())
}

fn check_part(i: u32, part: &Part<'_>) -> Result<(), GlbError> {
    let bad = |field| GlbError::BadValue { part: i, field };
    if let Some(p) = part.parent
        && p >= i
    {
        return Err(GlbError::ParentOrder { part: i, parent: p });
    }
    match part.transform {
        Transform::Trs(t) => {
            if !finite3(t.translation) {
                return Err(bad(Field::Translation));
            }
            if !t.rotation.iter().all(|x| x.is_finite()) {
                return Err(bad(Field::Rotation));
            }
            let r = t.rotation;
            let length = blaze_cad_mesh::geom::sqrt(r[0] * r[0] + r[1] * r[1] + r[2] * r[2] + r[3] * r[3]);
            if (length - 1.0).abs() > 1.0 / 1_048_576.0 {
                return Err(GlbError::NotUnitRotation { part: i, length });
            }
            if !finite3(t.scale) || t.scale.contains(&0.0) {
                return Err(bad(Field::Scale));
            }
        }
        Transform::Matrix(m) => {
            if !finite3(m.translation) {
                return Err(bad(Field::Translation));
            }
            let det = m.det();
            if !(m.linear.iter().all(|row| finite3(*row)) && det.is_finite() && det != 0.0) {
                return Err(bad(Field::Matrix));
            }
        }
    }
    if let Some(m) = part.mass {
        if !(m.mass.is_finite() && m.mass >= 0.0) {
            return Err(bad(Field::Mass));
        }
        if !finite3(m.centre) {
            return Err(bad(Field::Centre));
        }
        if !m.inertia.iter().all(|row| finite3(*row)) {
            return Err(bad(Field::Inertia));
        }
    }
    if let Some(j) = part.joint {
        if !finite3(j.axis) {
            return Err(bad(Field::JointAxis));
        }
        if let Some([lo, hi]) = j.limits
            && !(lo.is_finite() && hi.is_finite() && lo <= hi)
        {
            return Err(bad(Field::JointLimits));
        }
    }
    match part.collider {
        Some(Collider::Box { centre, half }) => {
            if !finite3(centre) {
                return Err(bad(Field::ColliderCentre));
            }
            if !(finite3(half) && half.iter().all(|&h| h >= 0.0)) {
                return Err(bad(Field::ColliderSize));
            }
        }
        Some(Collider::Sphere { centre, radius }) => {
            if !finite3(centre) {
                return Err(bad(Field::ColliderCentre));
            }
            if !(radius.is_finite() && radius >= 0.0) {
                return Err(bad(Field::ColliderSize));
            }
        }
        Some(Collider::Hulls(_) | Collider::Mesh) | None => {}
    }
    for lod in part.lods {
        if lod.error < Length::ZERO {
            return Err(bad(Field::LodError));
        }
        if lod.switch_distance < Length::ZERO {
            return Err(bad(Field::LodSwitchDistance));
        }
    }
    Ok(())
}

fn check_material(i: u32, m: &Material<'_>) -> Result<(), GlbError> {
    let unit_range = |x: f64| x.is_finite() && (0.0..=1.0).contains(&x);
    let bad = |field| GlbError::BadMaterial { material: i, field };
    if !m.base_colour.iter().all(|&x| unit_range(x)) {
        return Err(bad(MaterialField::BaseColour));
    }
    if !unit_range(m.metallic) {
        return Err(bad(MaterialField::Metallic));
    }
    if !unit_range(m.roughness) {
        return Err(bad(MaterialField::Roughness));
    }
    if !m.emissive.iter().all(|&x| unit_range(x)) {
        return Err(bad(MaterialField::Emissive));
    }
    if let Some(e) = m.emissive_strength
        && !(e.is_finite() && e >= 0.0)
    {
        return Err(bad(MaterialField::EmissiveStrength));
    }
    if let Some(c) = m.clearcoat {
        if !unit_range(c.factor) {
            return Err(bad(MaterialField::Clearcoat));
        }
        if !unit_range(c.roughness) {
            return Err(bad(MaterialField::ClearcoatRoughness));
        }
    }
    Ok(())
}

/// The box of every position of every mesh of `part`, in glTF's frame; `None` when it has none.
fn part_box(part: &Part<'_>, up: Up) -> Option<Aabb> {
    let mut b = Aabb::EMPTY;
    let mut add = |view: &MeshView<'_>| {
        for &p in view.positions() {
            b.include(turn_p3(p, up));
        }
    };
    if let Some(view) = &part.mesh {
        add(view);
    }
    for lod in part.lods {
        add(&lod.mesh);
    }
    if let Some(Collider::Hulls(hulls)) = part.collider {
        for view in hulls {
            add(view);
        }
    }
    if b.is_empty() { None } else { Some(b) }
}

fn check_textures(scene: &Scene<'_>, textures: &TexturePack<'_>) -> Result<(), GlbError> {
    if textures.material_textures.len() > scene.materials.len() {
        return Err(GlbError::TooManyMaterialTextures {
            got: textures.material_textures.len(),
            materials: scene.materials.len(),
        });
    }
    for (i, &img) in textures.png_images.iter().enumerate() {
        if img.len() < PNG_SIGNATURE.len() || img[..PNG_SIGNATURE.len()] != PNG_SIGNATURE {
            return Err(GlbError::InvalidPngImage { image: count32(i)? });
        }
    }
    let count = textures.png_images.len();
    for (i, mt) in textures.material_textures.iter().enumerate() {
        let material = count32(i)?;
        if !(mt.normal_scale.is_finite() && mt.normal_scale >= 0.0) {
            return Err(GlbError::BadMaterial { material, field: MaterialField::NormalScale });
        }
        for opt in [mt.base_colour, mt.metallic_roughness, mt.normal] {
            if let Some(index) = opt
                && idx(index) >= count
            {
                return Err(GlbError::UnknownTexture { material, index, count });
            }
        }
    }
    Ok(())
}

/// [`glb`] with a planted bug, for the tests' controls.
pub fn glb_planted<'a>(
    scene: &Scene<'_>,
    opts: GlbOpts<'a, '_>,
    plant: impl Planted<GlbPlant>,
) -> Result<Vec<u8, Alloc<'a>>, GlbError> {
    glb_textured_planted(scene, &TexturePack::EMPTY, opts, plant)
}

/// [`glb_textured`] with a planted bug, for the tests' controls.
pub fn glb_textured_planted<'a>(
    scene: &Scene<'_>,
    textures: &TexturePack<'_>,
    opts: GlbOpts<'a, '_>,
    plant: impl Planted<GlbPlant>,
) -> Result<Vec<u8, Alloc<'a>>, GlbError> {
    let s = opts.scratch;
    let parts = scene.parts;
    if parts.len() > idx(opts.limits.most_parts) {
        return Err(GlbError::TooManyParts { parts: parts.len(), most: opts.limits.most_parts });
    }
    for (i, m) in scene.materials.iter().enumerate() {
        check_material(count32(i)?, m)?;
    }
    check_textures(scene, textures)?;
    for (i, part) in parts.iter().enumerate() {
        check_part(count32(i)?, part)?;
    }
    let quantized = matches!(opts.encoding, Encoding::Quantized { .. });
    let n = parts.len();

    // Node numbers: each part, then its `.mesh` node when quantized.
    let mut node_of: V<'_, u32> = Vec::try_with_capacity_in(n, s)?;
    let mut next = 0u32;
    for part in parts {
        node_of.try_push(next).map_err(|r| r.why)?;
        next += 1 + u32::from(quantized && part.mesh.is_some());
    }
    // Children by parent, in scene order (a counting sort).
    let mut start: V<'_, u32> = Vec::try_with_capacity_in(n + 1, s)?;
    for _ in 0..=n {
        start.try_push(0).map_err(|r| r.why)?;
    }
    for part in parts {
        if let Some(p) = part.parent {
            start[idx(p) + 1] += 1;
        }
    }
    for i in 0..n {
        start[i + 1] += start[i];
    }
    let mut fill: V<'_, u32> = Vec::try_with_capacity_in(n, s)?;
    let mut kids: V<'_, u32> = Vec::try_with_capacity_in(idx(start[n]), s)?;
    for i in 0..n {
        fill.try_push(start[i]).map_err(|r| r.why)?;
    }
    for _ in 0..start[n] {
        kids.try_push(0).map_err(|r| r.why)?;
    }
    for (i, part) in parts.iter().enumerate() {
        if let Some(p) = part.parent {
            kids[idx(fill[idx(p)])] = count32(i)?;
            fill[idx(p)] += 1;
        }
    }

    // The BIN chunk, part by part: main mesh, levels of detail, hulls.
    let mut bin = Bin {
        bytes: Vec::new_in(s),
        views: Vec::new_in(s),
        accs: Vec::new_in(s),
        prims: Vec::new_in(s),
        meshes: Vec::new_in(s),
        scratch: s,
        plant,
        fired: [false; 3],
    };
    let mut recs: V<'_, PartRec> = Vec::try_with_capacity_in(n, s)?;
    for (i, part) in parts.iter().enumerate() {
        let pi = count32(i)?;
        let within = if quantized { part_box(part, opts.up) } else { None };
        let mut ctx = MeshCtx {
            part: pi,
            slot: MeshSlot::Main,
            up: opts.up,
            encoding: opts.encoding,
            within,
            materials: Some(scene.materials),
        };
        let mut dequant = None;
        let mut note = |d: Option<Dequantize>| {
            if dequant.is_none() {
                dequant = d.map(|d| (d.offset, node_scale(d)));
            }
        };
        let main = match &part.mesh {
            Some(view) => {
                let (m, d) = put_mesh(&mut bin, view, &ctx)?;
                note(d);
                Some(m)
            }
            None => None,
        };
        let lod_start = count32(bin.meshes.len())?;
        for (k, lod) in part.lods.iter().enumerate() {
            ctx.slot = MeshSlot::Lod(count32(k + 1)?);
            let (_, d) = put_mesh(&mut bin, &lod.mesh, &ctx)?;
            note(d);
        }
        let hull_start = count32(bin.meshes.len())?;
        if let Some(Collider::Hulls(hulls)) = part.collider {
            ctx.materials = None;
            for (k, view) in hulls.iter().enumerate() {
                ctx.slot = MeshSlot::Hull(count32(k)?);
                let (_, d) = put_mesh(&mut bin, view, &ctx)?;
                note(d);
            }
        }
        let hull_end = count32(bin.meshes.len())?;
        recs.try_push(PartRec { main, lods: (lod_start, hull_start), hulls: (hull_start, hull_end), dequant })
            .map_err(|r| r.why)?;
    }
    let mut image_views: V<'_, u32> = Vec::try_with_capacity_in(textures.png_images.len(), s)?;
    for &img in textures.png_images {
        let start = bin.begin_view()?;
        bin.put(img)?;
        let iv = bin.end_view(start, 0, None)?;
        image_views.try_push(iv).map_err(|r| r.why)?;
    }
    bin.pad()?;

    // The JSON chunk.
    let mut text = Bytes::new(s);
    {
        let doc = Doc {
            scene,
            material_textures: textures.material_textures,
            image_views: &image_views,
            quantized,
            up: opts.up,
            node_of: &node_of,
            start: &start,
            kids: &kids,
            recs: &recs,
            meshes: &bin.meshes,
            prims: &bin.prims,
            accs: &bin.accs,
            views: &bin.views,
            bin_len: bin.bytes.len(),
        };
        let mut j = Json::new(&mut text);
        doc.write(&mut j);
        j.finish().map_err(GlbError::Json)?;
    }
    let text = text.finish()?;

    // The container.
    let json_padded = text.len() + (4 - text.len() % 4) % 4;
    let bin_len = bin.bytes.len();
    let bin_part = if bin_len > 0 { 8 + bin_len } else { 0 };
    let total: u64 = 12u64 + 8 + json_padded.widen_to::<u64>() + bin_part.widen_to::<u64>();
    let most = u64::from(opts.limits.most_glb_bytes);
    if total > most {
        return Err(GlbError::TooLarge { bytes: total, most });
    }
    // At most `most_glb_bytes`, a `u32`.
    let total32: u32 = total.narrow_or_stop();
    let mut out = Vec::try_with_capacity_in(idx(total32), opts.alloc)?;
    out.try_extend_from_slice(b"glTF")?;
    out.try_extend_from_slice(&2u32.to_le_bytes())?;
    out.try_extend_from_slice(&total32.to_le_bytes())?;
    out.try_extend_from_slice(&count32(json_padded)?.to_le_bytes())?;
    out.try_extend_from_slice(b"JSON")?;
    out.try_extend_from_slice(&text)?;
    while out.len() % 4 != 0 {
        out.try_extend_from_slice(b" ")?;
    }
    if bin_len > 0 {
        out.try_extend_from_slice(&count32(bin_len)?.to_le_bytes())?;
        out.try_extend_from_slice(b"BIN\0")?;
        out.try_extend_from_slice(&bin.bytes)?;
    }
    Ok(out)
}

/// What the JSON chunk is written from.
struct Doc<'d> {
    scene: &'d Scene<'d>,
    material_textures: &'d [MaterialTextures],
    image_views: &'d [u32],
    quantized: bool,
    up: Up,
    node_of: &'d [u32],
    start: &'d [u32],
    kids: &'d [u32],
    recs: &'d [PartRec],
    meshes: &'d [MeshRec],
    prims: &'d [PrimRec],
    accs: &'d [AccRec],
    views: &'d [ViewRec],
    bin_len: usize,
}

impl Doc<'_> {
    fn write<W: Put>(&self, j: &mut Json<'_, W>) {
        let scene = self.scene;
        j.map();
        j.key("asset").map();
        j.key("generator").str(GENERATOR);
        j.key("version").str("2.0");
        j.end_map();
        let clearcoat = scene.materials.iter().any(|m| m.clearcoat.is_some());
        let strength = scene.materials.iter().any(|m| m.emissive_strength.is_some());
        let quant = self.quantized && !self.meshes.is_empty();
        if clearcoat || strength || quant {
            j.key("extensionsUsed").list();
            if clearcoat {
                j.str("KHR_materials_clearcoat");
            }
            if strength {
                j.str("KHR_materials_emissive_strength");
            }
            if quant {
                j.str("KHR_mesh_quantization");
            }
            j.end_list();
        }
        if quant {
            j.key("extensionsRequired").list().str("KHR_mesh_quantization").end_list();
        }
        j.key("scene").uint(0);
        j.key("scenes").list().map();
        if scene.parts.iter().any(|p| p.parent.is_none()) {
            j.key("nodes").list();
            for (i, part) in scene.parts.iter().enumerate() {
                if part.parent.is_none() {
                    j.uint(u64::from(self.node_of[i]));
                }
            }
            j.end_list();
        }
        j.end_map().end_list();
        if !scene.parts.is_empty() {
            j.key("nodes").list();
            for (i, part) in scene.parts.iter().enumerate() {
                self.node(j, i, part);
            }
            j.end_list();
        }
        if !self.meshes.is_empty() {
            j.key("meshes").list();
            for m in self.meshes {
                let name = scene.parts[idx(m.part)].name;
                j.map().key("name");
                match m.slot {
                    MeshSlot::Main => j.str(name),
                    MeshSlot::Lod(k) => j.str_from(name, ".lod", Some(k)),
                    MeshSlot::Hull(k) => j.str_from(name, ".hull", Some(k)),
                };
                j.key("primitives").list();
                for p in &self.prims[idx(m.prims.0)..idx(m.prims.1)] {
                    j.map();
                    j.key("attributes").map();
                    j.key("POSITION").uint(u64::from(m.position));
                    j.key("NORMAL").uint(u64::from(m.normal));
                    if let Some(texcoord) = m.texcoord {
                        j.key("TEXCOORD_0").uint(u64::from(texcoord));
                    }
                    if let Some(tangent) = m.tangent {
                        j.key("TANGENT").uint(u64::from(tangent));
                    }
                    j.end_map();
                    j.key("indices").uint(u64::from(p.indices));
                    if let Some(material) = p.material {
                        j.key("material").uint(u64::from(material));
                    }
                    j.end_map();
                }
                j.end_list().end_map();
            }
            j.end_list();
        }
        if !scene.materials.is_empty() {
            j.key("materials").list();
            for (i, m) in scene.materials.iter().enumerate() {
                material(j, m, self.material_textures.get(i).copied());
            }
            j.end_list();
        }
        if !self.image_views.is_empty() {
            j.key("textures").list();
            for i in 0..self.image_views.len() {
                j.map();
                j.key("sampler").uint(0);
                j.key("source").uint(i.widen_to());
                j.end_map();
            }
            j.end_list();
            j.key("images").list();
            for &view in self.image_views {
                j.map();
                j.key("bufferView").uint(u64::from(view));
                j.key("mimeType").str("image/png");
                j.end_map();
            }
            j.end_list();
            j.key("samplers").list().map();
            j.key("magFilter").uint(MAG_LINEAR);
            j.key("minFilter").uint(MIN_LINEAR_MIPMAP_LINEAR);
            j.key("wrapS").uint(WRAP_REPEAT);
            j.key("wrapT").uint(WRAP_REPEAT);
            j.end_map().end_list();
        }
        if !self.accs.is_empty() {
            j.key("accessors").list();
            for a in self.accs {
                j.map();
                j.key("bufferView").uint(u64::from(a.view));
                j.key("componentType").uint(a.component);
                if a.normalized {
                    j.key("normalized").bool(true);
                }
                j.key("count").uint(u64::from(a.count));
                j.key("type").str(a.kind.name());
                match a.bounds {
                    Bounds::None => {}
                    Bounds::F32 { min, max } => {
                        j.key("min").nums(&wide(min));
                        j.key("max").nums(&wide(max));
                    }
                    Bounds::U16 { min, max } => {
                        j.key("min").uints(min.map(u64::from));
                        j.key("max").uints(max.map(u64::from));
                    }
                }
                j.end_map();
            }
            j.end_list();
        }
        if !self.views.is_empty() {
            j.key("bufferViews").list();
            for v in self.views {
                j.map();
                j.key("buffer").uint(0);
                j.key("byteOffset").uint(u64::from(v.offset));
                j.key("byteLength").uint(u64::from(v.length));
                if v.stride != 0 {
                    j.key("byteStride").uint(u64::from(v.stride));
                }
                if let Some(target) = v.target {
                    j.key("target").uint(target);
                }
                j.end_map();
            }
            j.end_list();
        }
        if self.bin_len != 0 {
            j.key("buffers").list().map();
            j.key("byteLength").uint(self.bin_len.widen_to());
            j.end_map().end_list();
        }
        j.end_map();
    }

    fn node<W: Put>(&self, j: &mut Json<'_, W>, i: usize, part: &Part<'_>) {
        let rec = self.recs[i];
        let up = self.up;
        let mesh_node = self.quantized && rec.main.is_some();
        let kids = &self.kids[idx(self.start[i])..idx(self.start[i + 1])];
        j.map();
        j.key("name").str(part.name);
        if mesh_node || !kids.is_empty() {
            j.key("children").list();
            if mesh_node {
                j.uint(u64::from(self.node_of[i]) + 1);
            }
            for &k in kids {
                j.uint(u64::from(self.node_of[idx(k)]));
            }
            j.end_list();
        }
        match part.transform {
            Transform::Trs(t) => {
                let translation = turn(t.translation, up);
                if translation != [0.0; 3] {
                    j.key("translation").nums(&translation);
                }
                let rotation = turn_rotation(t.rotation, up);
                if rotation != [0.0, 0.0, 0.0, 1.0] {
                    j.key("rotation").nums(&rotation);
                }
                let scale = turn_extent(t.scale, up);
                if scale != [1.0; 3] {
                    j.key("scale").nums(&scale);
                }
            }
            Transform::Matrix(m) => {
                // The frame change of a map: `C · L · Cᵀ` and the translation turned, exact.
                let turned = Matrix { linear: turn_tensor(m.linear, up), translation: turn(m.translation, up) };
                j.key("matrix").nums(&turned.column_major());
            }
        }
        if !self.quantized
            && let Some(m) = rec.main
        {
            j.key("mesh").uint(u64::from(m));
        }
        j.key("extras");
        self.extras(j, i, part);
        j.end_map();
        if mesh_node && let Some(m) = rec.main {
            j.map();
            j.key("name").str_from(part.name, ".mesh", None);
            if let Some((offset, scale)) = rec.dequant {
                if offset != [0.0; 3] {
                    j.key("translation").nums(&wide(offset));
                }
                if scale != [1.0; 3] {
                    j.key("scale").nums(&wide(scale));
                }
            }
            j.key("mesh").uint(u64::from(m));
            j.end_map();
        }
    }

    fn extras<W: Put>(&self, j: &mut Json<'_, W>, i: usize, part: &Part<'_>) {
        let rec = self.recs[i];
        let up = self.up;
        j.map();
        j.key("name").str(part.name);
        j.key("roles").list();
        if let Some(m) = rec.main {
            // One primitive per role, in role order.
            let mesh = self.meshes[idx(m)];
            for p in &self.prims[idx(mesh.prims.0)..idx(mesh.prims.1)] {
                if let Some(material) = p.material {
                    j.str(self.scene.materials[idx(material)].name);
                }
            }
        }
        j.end_list();
        if let Some(mass) = part.mass {
            j.key("mass").num(mass.mass);
            j.key("centre").nums(&turn(mass.centre, up));
            j.key("inertia").list();
            for row in turn_tensor(mass.inertia, up) {
                j.nums(&row);
            }
            j.end_list();
        }
        if let Some(joint) = part.joint {
            j.key("joint").map();
            j.key("name").str(joint.name);
            j.key("kind").str(joint.kind.name());
            j.key("axis").nums(&turn(joint.axis, up));
            if let Some([lower, upper]) = joint.limits {
                j.key("lower").num(lower);
                j.key("upper").num(upper);
            }
            j.end_map();
        }
        if let Some(collider) = part.collider {
            j.key("collider").map();
            match collider {
                Collider::Box { centre, half } => {
                    j.key("kind").str("box");
                    j.key("centre").nums(&turn(centre, up));
                    j.key("half").nums(&turn_extent(half, up));
                }
                Collider::Sphere { centre, radius } => {
                    j.key("kind").str("sphere");
                    j.key("centre").nums(&turn(centre, up));
                    j.key("radius").num(radius);
                }
                Collider::Hulls(_) => {
                    j.key("kind").str("hulls");
                    j.key("meshes").uints((rec.hulls.0..rec.hulls.1).map(u64::from));
                }
                Collider::Mesh => {
                    j.key("kind").str("mesh");
                    if let Some(m) = rec.main {
                        j.key("mesh").uint(u64::from(m));
                    }
                }
            }
            j.end_map();
        }
        if !part.lods.is_empty() {
            j.key("lod").list();
            for (k, lod) in part.lods.iter().enumerate() {
                j.map();
                j.key("mesh").uint(u64::from(rec.lods.0) + k.widen_to::<u64>());
                j.key("error").num(lod.error.as_m());
                j.key("switch_distance").num(lod.switch_distance.as_m());
                j.end_map();
            }
            j.end_list();
        }
        if let Some((offset, scale)) = rec.dequant {
            j.key("dequantize").map();
            j.key("translation").nums(&wide(offset));
            j.key("scale").nums(&wide(scale));
            j.end_map();
        }
        j.end_map();
    }
}

fn material<W: Put>(j: &mut Json<'_, W>, m: &Material<'_>, textures: Option<MaterialTextures>) {
    j.map();
    j.key("name").str(m.name);
    j.key("pbrMetallicRoughness").map();
    j.key("baseColorFactor").nums(&m.base_colour);
    if let Some(tex) = textures.and_then(|t| t.base_colour) {
        j.key("baseColorTexture").map().key("index").uint(u64::from(tex)).end_map();
    }
    j.key("metallicFactor").num(m.metallic);
    j.key("roughnessFactor").num(m.roughness);
    if let Some(tex) = textures.and_then(|t| t.metallic_roughness) {
        j.key("metallicRoughnessTexture").map().key("index").uint(u64::from(tex)).end_map();
    }
    j.end_map();
    if let Some(t) = textures
        && let Some(tex) = t.normal
    {
        j.key("normalTexture").map();
        j.key("index").uint(u64::from(tex));
        if t.normal_scale != 1.0 {
            j.key("scale").num(t.normal_scale);
        }
        j.end_map();
    }
    if m.emissive != [0.0; 3] {
        j.key("emissiveFactor").nums(&m.emissive);
    }
    if m.base_colour[3] < 1.0 {
        j.key("alphaMode").str("BLEND");
    }
    if m.double_sided {
        j.key("doubleSided").bool(true);
    }
    if m.clearcoat.is_some() || m.emissive_strength.is_some() {
        j.key("extensions").map();
        if let Some(c) = m.clearcoat {
            j.key("KHR_materials_clearcoat").map();
            j.key("clearcoatFactor").num(c.factor);
            j.key("clearcoatRoughnessFactor").num(c.roughness);
            j.end_map();
        }
        if let Some(e) = m.emissive_strength {
            j.key("KHR_materials_emissive_strength").map();
            j.key("emissiveStrength").num(e);
            j.end_map();
        }
        j.end_map();
    }
    j.end_map();
}
