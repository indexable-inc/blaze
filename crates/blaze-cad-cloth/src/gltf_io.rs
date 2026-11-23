//! Pure `#![no_std]` sans-IO binary glTF 2.0 (`.glb`) reader and deterministic skinned `.glb`
//! writer.
//!
//! - [`read_glb_skeleton_mesh`]: Reads a rigged binary glTF 2.0 character (`spacer.glb`) or rigid
//!   prop (`mag_boots.glb`, `wrist_unit.glb`), extracting positions, triangles, 57-joint skeleton
//!   hierarchy, `inverseBindMatrices`, and per-vertex `JOINTS_0` / `WEIGHTS_0` with zero external
//!   dependencies (uses `blaze-json`).
//! - [`write_skinned_glb`]: Emits a byte-for-byte deterministic skinned binary glTF 2.0 `.glb`
//!   carrying `POSITION`, `NORMAL`, `TEXCOORD_0`, `JOINTS_0`, `WEIGHTS_0`, semantic PBR materials
//!   (`mat_jacket`, `mat_trousers`, `mat_undershirt`, `mat_cordura`, `mat_leather`, `mat_metal`,
//!   `mat_dark`), and the full skeleton `skins[0]` + `nodes` hierarchy — directly consumable by
//!   ASTRA's `import_metahuman.py` with zero Blender in the loop!

#![blaze::sans_io]

use blaze_data::Value;
use blaze_math::Vec3;
use blaze_own::{Alloc, AllocFailure, Vec};
use blaze_text::{Put, Writes};

use crate::detail::{GarmentMesh, PanelKind};
use crate::fabric::Fabric;

/// Errors returned by [`read_glb_skeleton_mesh`] or [`write_skinned_glb`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GltfIoError {
    Alloc(AllocFailure),
    BadHeader,
    MissingJsonChunk,
    MissingBinChunk,
    JsonParse,
    MissingMesh { index: usize },
    UnsupportedAccessor,
    OutOfBounds,
}

impl From<AllocFailure> for GltfIoError {
    fn from(e: AllocFailure) -> Self {
        Self::Alloc(e)
    }
}

/// One joint in a glTF skeleton hierarchy.
pub struct JointDef<'a> {
    pub name: Vec<u8, Alloc<'a>>,
    /// Index of parent joint inside `Skeleton::joints`, or `None` for root joints.
    pub parent: Option<u32>,
    pub translation: [f64; 3],
    pub rotation: [f64; 4],
    pub scale: [f64; 3],
    /// Column-major 4×4 `inverseBindMatrix` from glTF `skins[0].inverseBindMatrices`.
    pub inverse_bind: [f32; 16],
}

/// A parsed glTF mesh + optional skin skeleton.
pub struct LoadedGlb<'a> {
    pub positions: Vec<Vec3, Alloc<'a>>,
    pub triangles: Vec<[u32; 3], Alloc<'a>>,
    /// Per-vertex 4 joint indices (into `joints`); empty if the mesh has no skin.
    pub joints_0: Vec<[u16; 4], Alloc<'a>>,
    /// Per-vertex 4 normalized joint weights; empty if the mesh has no skin.
    pub weights_0: Vec<[f32; 4], Alloc<'a>>,
    /// Skeleton joints in `skins[0].joints` order.
    pub joints: Vec<JointDef<'a>, Alloc<'a>>,
}

/// One named garment panel metadata entry decoded from a `.glb` file's `extras.blaze_panels`.
pub struct LoadedPanelEntry<'a> {
    pub id: u16,
    pub name: Vec<u8, Alloc<'a>>,
    pub kind: PanelKind,
    pub layer: u8,
    pub fabric_idx: u16,
    pub host_panel: Option<Vec<u8, Alloc<'a>>>,
}

impl LoadedPanelEntry<'_> {
    #[inline]
    pub fn name_str(&self) -> &str {
        core::str::from_utf8(&self.name).unwrap_or("")
    }

    #[inline]
    pub fn host_panel_str(&self) -> Option<&str> {
        self.host_panel.as_ref().and_then(|v| core::str::from_utf8(v).ok())
    }
}

/// Named panel table + per-triangle panel IDs decoded from an exported `.glb` file.
pub struct LoadedPanelMap<'a> {
    pub panels: Vec<LoadedPanelEntry<'a>, Alloc<'a>>,
    /// Per-triangle panel ID in primitive-emitted triangle order.
    pub tri_panel: Vec<u16, Alloc<'a>>,
}

impl LoadedPanelMap<'_> {
    /// Look up the [`LoadedPanelEntry`] owning exported triangle `tri_idx`.
    pub fn panel_for_triangle(&self, tri_idx: usize) -> Option<&LoadedPanelEntry<'_>> {
        let pid = *self.tri_panel.get(tri_idx)?;
        self.panels.iter().find(|p| p.id == pid)
    }
}

#[inline]
fn push<T>(v: &mut Vec<T, Alloc<'_>>, x: T) -> Result<(), GltfIoError> {
    v.try_push(x).map_err(|e| GltfIoError::Alloc(e.why))
}

#[inline]
fn read_u32_le(bytes: &[u8], off: usize) -> Result<u32, GltfIoError> {
    bytes.get(off..).and_then(<[u8]>::first_chunk).copied().map(u32::from_le_bytes).ok_or(GltfIoError::OutOfBounds)
}

#[inline]
fn read_u16_le(bytes: &[u8], off: usize) -> Result<u16, GltfIoError> {
    bytes.get(off..).and_then(<[u8]>::first_chunk).copied().map(u16::from_le_bytes).ok_or(GltfIoError::OutOfBounds)
}

#[inline]
fn read_f32_le(bytes: &[u8], off: usize) -> Result<f32, GltfIoError> {
    read_u32_le(bytes, off).map(f32::from_bits)
}

fn val_u64<A: core::alloc::Allocator>(v: &Value<A>) -> Option<u64> {
    match *v {
        Value::Int(i) => i.as_u64(),
        _ => None,
    }
}

fn val_f64<A: core::alloc::Allocator>(v: &Value<A>) -> Option<f64> {
    match *v {
        Value::Float(f) => Some(f),
        Value::Int(i) => Some(i.as_i128() as f64),
        _ => None,
    }
}

fn val_str<A: core::alloc::Allocator>(v: &Value<A>) -> Option<&str> {
    match v {
        Value::Text(s) => Some(s.as_str()),
        _ => None,
    }
}

fn val_map_get<'v, A: core::alloc::Allocator>(v: &'v Value<A>, key: &str) -> Option<&'v Value<A>> {
    match v {
        Value::Map(m) => m.get(key),
        _ => None,
    }
}

fn val_array<'v, A: core::alloc::Allocator>(v: &'v Value<A>) -> Option<&'v [Value<A>]> {
    match v {
        Value::List(items) => Some(items.as_slice()),
        _ => None,
    }
}

struct AccessorSlice<'a> {
    data: &'a [u8],
    count: usize,
    stride: usize,
    component_type: u64,
    components: usize,
}

fn accessor_size(count: usize, stride: usize, element_bytes: usize) -> Result<usize, GltfIoError> {
    if stride < element_bytes || count == 0 {
        return Err(GltfIoError::UnsupportedAccessor);
    }
    (count - 1).checked_mul(stride).and_then(|offset| offset.checked_add(element_bytes))
        .ok_or(GltfIoError::OutOfBounds)
}

#[cfg(test)]
mod accessor_tests {
    use super::{accessor_size, GltfIoError};

    #[test]
    fn strided_extent_ends_after_the_last_element() {
        for count in 1..8 {
            for size in 1..17 {
                for padding in 0..4 {
                    let stride = size + padding;
                    let end = (0..count).map(|i| i * stride + size).max();
                    assert_eq!(accessor_size(count, stride, size).ok(), end);
                }
            }
        }
        assert_eq!(accessor_size(2, usize::MAX, 4), Err(GltfIoError::OutOfBounds));
        assert_eq!(accessor_size(3, 2, 4), Err(GltfIoError::UnsupportedAccessor));
    }
}

fn resolve_accessor<'a, A: core::alloc::Allocator>(
    root: &Value<A>,
    bin: &'a [u8],
    acc_idx: usize,
) -> Result<AccessorSlice<'a>, GltfIoError> {
    let accessors = val_map_get(root, "accessors").and_then(val_array).ok_or(GltfIoError::UnsupportedAccessor)?;
    let acc = accessors.get(acc_idx).ok_or(GltfIoError::OutOfBounds)?;
    let bv_idx = val_map_get(acc, "bufferView").and_then(val_u64).ok_or(GltfIoError::UnsupportedAccessor)? as usize;
    let count = val_map_get(acc, "count").and_then(val_u64).ok_or(GltfIoError::UnsupportedAccessor)? as usize;
    let component_type = val_map_get(acc, "componentType").and_then(val_u64).ok_or(GltfIoError::UnsupportedAccessor)?;
    let acc_off = val_map_get(acc, "byteOffset").and_then(val_u64).unwrap_or(0) as usize;
    let typ = val_map_get(acc, "type").and_then(val_str).ok_or(GltfIoError::UnsupportedAccessor)?;
    let components = match typ {
        "SCALAR" => 1,
        "VEC2" => 2,
        "VEC3" => 3,
        "VEC4" => 4,
        "MAT4" => 16,
        _ => return Err(GltfIoError::UnsupportedAccessor),
    };
    let comp_bytes = match component_type {
        5121 => 1, // UNSIGNED_BYTE
        5123 => 2, // UNSIGNED_SHORT
        5125 | 5126 => 4, // UNSIGNED_INT / FLOAT
        _ => return Err(GltfIoError::UnsupportedAccessor),
    };

    let bvs = val_map_get(root, "bufferViews").and_then(val_array).ok_or(GltfIoError::UnsupportedAccessor)?;
    let bv = bvs.get(bv_idx).ok_or(GltfIoError::OutOfBounds)?;
    let bv_off = val_map_get(bv, "byteOffset").and_then(val_u64).unwrap_or(0) as usize;
    let bv_len = val_map_get(bv, "byteLength").and_then(val_u64).ok_or(GltfIoError::UnsupportedAccessor)? as usize;
    let stride = val_map_get(bv, "byteStride").and_then(val_u64).unwrap_or((components * comp_bytes) as u64) as usize;

    let data = bin.get(bv_off..).and_then(|b| b.get(..bv_len)).and_then(|b| b.get(acc_off..))
        .ok_or(GltfIoError::OutOfBounds)?;
    let element_bytes = components * comp_bytes;
    let last_end = accessor_size(count, stride, element_bytes)?;
    if last_end > data.len() {
        return Err(GltfIoError::OutOfBounds);
    }
    Ok(AccessorSlice { data, count, stride, component_type, components })
}

fn parse_glb_chunks(glb_bytes: &[u8]) -> Result<(&[u8], &[u8]), GltfIoError> {
    if glb_bytes.len() < 12 || !glb_bytes.starts_with(b"glTF") || read_u32_le(glb_bytes, 4)? != 2
        || usize::try_from(read_u32_le(glb_bytes, 8)?).ok() != Some(glb_bytes.len()) {
        return Err(GltfIoError::BadHeader);
    }
    let mut off = 12usize;
    let mut json_chunk: Option<&[u8]> = None;
    let mut bin_chunk: Option<&[u8]> = None;
    while off < glb_bytes.len() {
        let len = read_u32_le(glb_bytes, off)? as usize;
        let header = glb_bytes.get(off..).and_then(<[u8]>::first_chunk::<8>).ok_or(GltfIoError::OutOfBounds)?;
        let typ = &header[4..];
        off += header.len();
        let chunk = glb_bytes[off..].get(..len).ok_or(GltfIoError::OutOfBounds)?;
        if typ == b"JSON" {
            json_chunk = Some(chunk);
        } else if typ == b"BIN\0" {
            bin_chunk = Some(chunk);
        }
        off += len;
    }
    let json_bytes = json_chunk.ok_or(GltfIoError::MissingJsonChunk)?;
    let bin = bin_chunk.ok_or(GltfIoError::MissingBinChunk)?;
    Ok((json_bytes, bin))
}

/// Parse a binary `.glb` file in memory, extracting mesh `mesh_index` (or all meshes if
/// `mesh_index == None`) along with `skins[0]` skeleton hierarchy & weights if present.
pub fn read_glb_skeleton_mesh<'a>(
    glb_bytes: &[u8],
    mesh_index: Option<usize>,
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
) -> Result<LoadedGlb<'a>, GltfIoError> {
    let (json_bytes, bin) = parse_glb_chunks(glb_bytes)?;
    let json_str = core::str::from_utf8(json_bytes).map_err(|_| GltfIoError::JsonParse)?;
    let root = blaze_json::read_in(json_str.as_bytes(), blaze_json::Limits::SHIPPED, scratch).map_err(|_| GltfIoError::JsonParse)?;

    let meshes = val_map_get(&root, "meshes").and_then(val_array).ok_or(GltfIoError::MissingMesh { index: 0 })?;

    let mut positions = Vec::new_in(alloc);
    let mut triangles = Vec::new_in(alloc);
    let mut joints_0 = Vec::new_in(alloc);
    let mut weights_0 = Vec::new_in(alloc);

    let mesh_range = match mesh_index {
        Some(idx) => {
            if idx >= meshes.len() {
                return Err(GltfIoError::MissingMesh { index: idx });
            }
            idx..(idx + 1)
        }
        None => 0..meshes.len(),
    };

    for mi in mesh_range {
        let m = &meshes[mi];
        let prims = val_map_get(m, "primitives").and_then(val_array).ok_or(GltfIoError::MissingMesh { index: mi })?;
        for prim in prims {
            let attrs = val_map_get(prim, "attributes").ok_or(GltfIoError::UnsupportedAccessor)?;
            let pos_idx = val_map_get(attrs, "POSITION").and_then(val_u64).ok_or(GltfIoError::UnsupportedAccessor)? as usize;
            let pos_acc = resolve_accessor(&root, bin, pos_idx)?;
            if pos_acc.component_type != 5126 || pos_acc.components != 3 {
                return Err(GltfIoError::UnsupportedAccessor);
            }
            let base_vert = u32::try_from(positions.len()).map_err(|_| GltfIoError::OutOfBounds)?;
            let vertex_count = u32::try_from(pos_acc.count).map_err(|_| GltfIoError::OutOfBounds)?;
            base_vert.checked_add(vertex_count).ok_or(GltfIoError::OutOfBounds)?;
            for i in 0..pos_acc.count {
                let o = i * pos_acc.stride;
                let x = read_f32_le(pos_acc.data, o)? as f64;
                let y = read_f32_le(pos_acc.data, o + 4)? as f64;
                let z = read_f32_le(pos_acc.data, o + 8)? as f64;
                push(&mut positions, Vec3::new(x, y, z))?;
            }

            let joints_attribute = val_map_get(attrs, "JOINTS_0");
            let weights_attribute = val_map_get(attrs, "WEIGHTS_0");
            if joints_attribute.is_some() != weights_attribute.is_some() {
                return Err(GltfIoError::UnsupportedAccessor);
            }
            if let (Some(joints_attribute), Some(weights_attribute)) = (joints_attribute, weights_attribute) {
                let j_idx = val_u64(joints_attribute).ok_or(GltfIoError::UnsupportedAccessor)?;
                let w_idx = val_u64(weights_attribute).ok_or(GltfIoError::UnsupportedAccessor)?;
                let j_acc = resolve_accessor(&root, bin, j_idx as usize)?;
                let w_acc = resolve_accessor(&root, bin, w_idx as usize)?;
                if j_acc.components != 4 || w_acc.components != 4
                    || j_acc.count != pos_acc.count || w_acc.count != pos_acc.count {
                    return Err(GltfIoError::UnsupportedAccessor);
                }
                for i in 0..pos_acc.count {
                    let jo = i * j_acc.stride;
                    let jv = match j_acc.component_type {
                        5121 => [
                            j_acc.data[jo] as u16,
                            j_acc.data[jo + 1] as u16,
                            j_acc.data[jo + 2] as u16,
                            j_acc.data[jo + 3] as u16,
                        ],
                        5123 => [
                            read_u16_le(j_acc.data, jo)?,
                            read_u16_le(j_acc.data, jo + 2)?,
                            read_u16_le(j_acc.data, jo + 4)?,
                            read_u16_le(j_acc.data, jo + 6)?,
                        ],
                        _ => return Err(GltfIoError::UnsupportedAccessor),
                    };
                    let wo = i * w_acc.stride;
                    let wv = match w_acc.component_type {
                        5126 => [
                            read_f32_le(w_acc.data, wo)?,
                            read_f32_le(w_acc.data, wo + 4)?,
                            read_f32_le(w_acc.data, wo + 8)?,
                            read_f32_le(w_acc.data, wo + 12)?,
                        ],
                        5121 => [
                            w_acc.data[wo] as f32 / 255.0,
                            w_acc.data[wo + 1] as f32 / 255.0,
                            w_acc.data[wo + 2] as f32 / 255.0,
                            w_acc.data[wo + 3] as f32 / 255.0,
                        ],
                        5123 => [
                            read_u16_le(w_acc.data, wo)? as f32 / 65535.0,
                            read_u16_le(w_acc.data, wo + 2)? as f32 / 65535.0,
                            read_u16_le(w_acc.data, wo + 4)? as f32 / 65535.0,
                            read_u16_le(w_acc.data, wo + 6)? as f32 / 65535.0,
                        ],
                        _ => return Err(GltfIoError::UnsupportedAccessor),
                    };
                    push(&mut joints_0, jv)?;
                    push(&mut weights_0, wv)?;
                }
            }

            if let Some(ind_idx) = val_map_get(prim, "indices").and_then(val_u64) {
                let i_acc = resolve_accessor(&root, bin, ind_idx as usize)?;
                if i_acc.components != 1 {
                    return Err(GltfIoError::UnsupportedAccessor);
                }
                let n_tris = i_acc.count / 3;
                for t in 0..n_tris {
                    let mut tri = [0u32; 3];
                    for c in 0..3 {
                        let o = (t * 3 + c) * i_acc.stride;
                        let idx = match i_acc.component_type {
                            5121 => i_acc.data[o] as u32,
                            5123 => read_u16_le(i_acc.data, o)? as u32,
                            5125 => read_u32_le(i_acc.data, o)?,
                            _ => return Err(GltfIoError::UnsupportedAccessor),
                        };
                        if idx >= vertex_count {
                            return Err(GltfIoError::OutOfBounds);
                        }
                        tri[c] = base_vert + idx;
                    }
                    push(&mut triangles, tri)?;
                }
            } else {
                let n_tris = pos_acc.count / 3;
                for t in 0..n_tris {
                    push(
                        &mut triangles,
                        [base_vert + (t * 3) as u32, base_vert + (t * 3 + 1) as u32, base_vert + (t * 3 + 2) as u32],
                    )?;
                }
            }
        }
    }

    // Parse skeleton `skins[0]` + `nodes` if present.
    let mut joints = Vec::new_in(alloc);
    if let (Some(skins), Some(nodes)) = (
        val_map_get(&root, "skins").and_then(val_array),
        val_map_get(&root, "nodes").and_then(val_array),
    ) {
        if let Some(skin0) = skins.first() {
            if let Some(joint_nodes) = val_map_get(skin0, "joints").and_then(val_array) {
                // Build node_index -> joint_index map.
                let mut node_to_joint = Vec::new_in(scratch);
                for _ in 0..nodes.len() {
                    push(&mut node_to_joint, u32::MAX)?;
                }
                for (ji, jv) in joint_nodes.iter().enumerate() {
                    if let Some(ni) = val_u64(jv) {
                        if (ni as usize) < node_to_joint.len() {
                            node_to_joint[ni as usize] = ji as u32;
                        }
                    }
                }
                // Build child -> parent map from node.children arrays.
                let mut node_parent = Vec::new_in(scratch);
                for _ in 0..nodes.len() {
                    push(&mut node_parent, u32::MAX)?;
                }
                for (ni, node) in nodes.iter().enumerate() {
                    if let Some(children) = val_map_get(node, "children").and_then(val_array) {
                        for ch in children {
                            if let Some(ci) = val_u64(ch) {
                                if (ci as usize) < node_parent.len() {
                                    node_parent[ci as usize] = ni as u32;
                                }
                            }
                        }
                    }
                }
                // Parse inverseBindMatrices if present.
                let ibm_acc = val_map_get(skin0, "inverseBindMatrices")
                    .and_then(val_u64)
                    .map(|idx| resolve_accessor(&root, bin, idx as usize))
                    .transpose()?;
                if let Some(acc) = &ibm_acc {
                    if acc.components != 16 || acc.component_type != 5126 || acc.count < joint_nodes.len() {
                        return Err(GltfIoError::UnsupportedAccessor);
                    }
                }

                for (ji, jv) in joint_nodes.iter().enumerate() {
                    let ni = val_u64(jv).ok_or(GltfIoError::UnsupportedAccessor)? as usize;
                    let node = nodes.get(ni).ok_or(GltfIoError::OutOfBounds)?;
                    let name_str = val_map_get(node, "name").and_then(val_str).unwrap_or("joint");
                    let mut name = Vec::new_in(alloc);
                    for &b in name_str.as_bytes() {
                        push(&mut name, b)?;
                    }
                    let mut translation = [0.0, 0.0, 0.0];
                    if let Some(arr) = val_map_get(node, "translation").and_then(val_array) {
                        for k in 0..3.min(arr.len()) {
                            translation[k] = val_f64(&arr[k]).unwrap_or(0.0);
                        }
                    }
                    let mut rotation = [0.0, 0.0, 0.0, 1.0];
                    if let Some(arr) = val_map_get(node, "rotation").and_then(val_array) {
                        for k in 0..4.min(arr.len()) {
                            rotation[k] = val_f64(&arr[k]).unwrap_or(if k == 3 { 1.0 } else { 0.0 });
                        }
                    }
                    let mut scale = [1.0, 1.0, 1.0];
                    if let Some(arr) = val_map_get(node, "scale").and_then(val_array) {
                        for k in 0..3.min(arr.len()) {
                            scale[k] = val_f64(&arr[k]).unwrap_or(1.0);
                        }
                    }
                    let mut inverse_bind = [
                        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
                    ];
                    if let Some(ref acc) = ibm_acc {
                        if ji < acc.count {
                            let o = ji * acc.stride;
                            for k in 0..16 {
                                inverse_bind[k] = read_f32_le(acc.data, o + k * 4)?;
                            }
                        }
                    }
                    let pnode = node_parent[ni];
                    let parent = if pnode != u32::MAX && node_to_joint[pnode as usize] != u32::MAX {
                        Some(node_to_joint[pnode as usize])
                    } else {
                        None
                    };
                    push(&mut joints, JointDef { name, parent, translation, rotation, scale, inverse_bind })?;
                }
            }
        }
    }

    if !joints_0.is_empty() && (joints_0.len() != positions.len()
        || joints_0.iter().flatten().any(|joint| usize::from(*joint) >= joints.len())) {
        return Err(GltfIoError::OutOfBounds);
    }
    Ok(LoadedGlb { positions, triangles, joints_0, weights_0, joints })
}

/// Read the embedded named panel table (`meshes[0].extras.blaze_panels`) and packed per-triangle
/// panel IDs (`tri_panel_accessor`) from an exported `.glb` binary buffer so in-game inspectors
/// can map `clicked_triangle -> panel_id -> CAD pattern piece name` with 100% fidelity.
pub fn read_glb_panel_metadata<'a>(
    glb_bytes: &[u8],
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
) -> Result<LoadedPanelMap<'a>, GltfIoError> {
    let (json_bytes, bin) = parse_glb_chunks(glb_bytes)?;
    let json_str = core::str::from_utf8(json_bytes).map_err(|_| GltfIoError::JsonParse)?;
    let root = blaze_json::read_in(json_str.as_bytes(), blaze_json::Limits::SHIPPED, scratch)
        .map_err(|_| GltfIoError::JsonParse)?;

    let mut panels = Vec::new_in(alloc);
    let mut tri_panel = Vec::new_in(alloc);

    let meshes = val_map_get(&root, "meshes").and_then(val_array).ok_or(GltfIoError::MissingMesh { index: 0 })?;
    let mesh0 = meshes.first().ok_or(GltfIoError::MissingMesh { index: 0 })?;
    let Some(extras) = val_map_get(mesh0, "extras") else {
        return Ok(LoadedPanelMap { panels, tri_panel });
    };
    let Some(bp) = val_map_get(extras, "blaze_panels") else {
        return Ok(LoadedPanelMap { panels, tri_panel });
    };

    if let Some(panel_list) = val_map_get(bp, "panels").and_then(val_array) {
        for entry in panel_list {
            let id = val_map_get(entry, "id").and_then(val_u64).ok_or(GltfIoError::UnsupportedAccessor)? as u16;
            let name_str = val_map_get(entry, "name").and_then(val_str).ok_or(GltfIoError::UnsupportedAccessor)?;
            let kind_str = val_map_get(entry, "kind").and_then(val_str).ok_or(GltfIoError::UnsupportedAccessor)?;
            let kind = PanelKind::from_str(kind_str).ok_or(GltfIoError::UnsupportedAccessor)?;
            let layer = val_map_get(entry, "layer").and_then(val_u64).ok_or(GltfIoError::UnsupportedAccessor)? as u8;
            let fabric_idx = val_map_get(entry, "material").and_then(val_u64).ok_or(GltfIoError::UnsupportedAccessor)? as u16;

            let mut name = Vec::new_in(alloc);
            for &b in name_str.as_bytes() {
                push(&mut name, b)?;
            }
            let host_panel = match val_map_get(entry, "host_panel").and_then(val_str) {
                Some(hs) => {
                    let mut hv = Vec::new_in(alloc);
                    for &b in hs.as_bytes() {
                        push(&mut hv, b)?;
                    }
                    Some(hv)
                }
                None => None,
            };
            push(&mut panels, LoadedPanelEntry { id, name, kind, layer, fabric_idx, host_panel })?;
        }
    }

    if let Some(acc_idx) = val_map_get(bp, "tri_panel_accessor").and_then(val_u64) {
        let acc = resolve_accessor(&root, bin, acc_idx as usize)?;
        if acc.components != 1 || acc.component_type != 5123 {
            return Err(GltfIoError::UnsupportedAccessor);
        }
        for i in 0..acc.count {
            let o = i * acc.stride;
            let pid = read_u16_le(acc.data, o)?;
            push(&mut tri_panel, pid)?;
        }
    }

    Ok(LoadedPanelMap { panels, tri_panel })
}

// ================================================================================================
// Deterministic Skinned Binary glTF 2.0 Writer
// ================================================================================================

struct JsonBuf<'a> {
    buf: Vec<u8, Alloc<'a>>,
    failed: Option<AllocFailure>,
}

impl<'a> JsonBuf<'a> {
    fn new(alloc: Alloc<'a>) -> Self {
        Self { buf: Vec::new_in(alloc), failed: None }
    }
    fn s(&mut self, text: &str) {
        if self.failed.is_some() {
            return;
        }
        for &b in text.as_bytes() {
            if let Err(e) = self.buf.try_push(b) {
                self.failed = Some(e.why);
                return;
            }
        }
    }
    fn u(&mut self, v: u64) {
        if self.failed.is_some() {
            return;
        }
        let mut sink = JPut(self);
        let _ = sink.int(v);
    }
    fn f(&mut self, v: f64) {
        if self.failed.is_some() {
            return;
        }
        let clean = if v == 0.0 { 0.0 } else { v };
        let mut sink = JPut(self);
        let _ = sink.float(clean);
    }
    fn q(&mut self, text: &str) {
        self.s("\"");
        for b in text.bytes() {
            match b {
                b'"' => self.s("\\\""),
                b'\\' => self.s("\\\\"),
                0x20..=0x7E => {
                    if self.failed.is_none() {
                        if let Err(e) = self.buf.try_push(b) {
                            self.failed = Some(e.why);
                        }
                    }
                }
                _ => self.s("_"),
            }
        }
        self.s("\"");
    }
}

struct JPut<'a, 'b>(&'b mut JsonBuf<'a>);

impl Put for JPut<'_, '_> {
    fn put_str(&mut self, s: &str) {
        if self.0.failed.is_some() {
            return;
        }
        for &b in s.as_bytes() {
            if let Err(e) = self.0.buf.try_push(b) {
                self.0.failed = Some(e.why);
                return;
            }
        }
    }
}

fn push_bytes(bin: &mut Vec<u8, Alloc<'_>>, src: &[u8]) -> Result<(), GltfIoError> {
    for &b in src {
        push(bin, b)?;
    }
    Ok(())
}

fn pad4(bin: &mut Vec<u8, Alloc<'_>>, pad_byte: u8) -> Result<(), GltfIoError> {
    while (bin.len() & 3) != 0 {
        push(bin, pad_byte)?;
    }
    Ok(())
}

/// One named glTF 2.0 morph target carrying per-vertex position deltas in bind-pose metres
/// (used for multi-pose corrective PSD blendshapes like `"rifle_aim"` and `"crouch"`).
pub struct MorphTarget<'a> {
    pub name: &'static str,
    pub position_deltas: &'a [Vec3],
}

/// Write a deterministic, byte-identical skinned binary glTF 2.0 (`.glb`) containing the full
/// garment mesh, `POSITION`, `NORMAL`, `TEXCOORD_0`, `COLOR_0` (normalized RGBA8 cloth intelligence),
/// `JOINTS_0`, `WEIGHTS_0`, optional pose-space morph `targets` (`rifle_aim`, `crouch`), one
/// primitive per fabric material role, and the complete skeleton hierarchy + `inverseBindMatrices`.
#[allow(clippy::too_many_arguments)]
pub fn write_skinned_glb_with_morphs<'a>(
    mesh: &GarmentMesh<'_>,
    fabrics: &[Fabric],
    joints: &[JointDef<'_>],
    skin_joints: &[[u16; 4]],
    skin_weights: &[[f32; 4]],
    morph_targets: &[MorphTarget<'_>],
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
) -> Result<Vec<u8, Alloc<'a>>, GltfIoError> {
    let n_verts = mesh.positions.len();
    let has_skin = !joints.is_empty() && skin_joints.len() == n_verts && skin_weights.len() == n_verts;
    let has_color = mesh.colors.len() == n_verts && n_verts > 0;
    let has_rest_pos = mesh.rest_pos_m.len() == n_verts && n_verts > 0;
    let has_panels = !mesh.panels.is_empty() && mesh.tri_panel.len() == mesh.triangles.len();

    let mut bin = Vec::new_in(scratch);
    struct BvMeta {
        offset: usize,
        length: usize,
        target: Option<u64>,
    }
    struct AccMeta {
        bv: usize,
        component_type: u64,
        count: usize,
        typ: &'static str,
        normalized: bool,
        min: Option<[f64; 3]>,
        max: Option<[f64; 3]>,
    }
    let mut bvs = Vec::new_in(scratch);
    let mut accs = Vec::new_in(scratch);

    // 0: POSITION (f32 VEC3)
    let pos_off = bin.len();
    let mut pmin = [f64::INFINITY; 3];
    let mut pmax = [f64::NEG_INFINITY; 3];
    for &p in mesh.positions.iter() {
        let f = [p.x as f32, p.y as f32, p.z as f32];
        for k in 0..3 {
            let v = f[k] as f64;
            if v < pmin[k] { pmin[k] = v; }
            if v > pmax[k] { pmax[k] = v; }
            push_bytes(&mut bin, &f[k].to_le_bytes())?;
        }
    }
    if n_verts == 0 {
        pmin = [0.0; 3];
        pmax = [0.0; 3];
    }
    let pos_len = bin.len() - pos_off;
    pad4(&mut bin, 0)?;
    let pos_bv = bvs.len();
    push(&mut bvs, BvMeta { offset: pos_off, length: pos_len, target: Some(34962) })?;
    let pos_acc = accs.len();
    push(&mut accs, AccMeta { bv: pos_bv, component_type: 5126, count: n_verts, typ: "VEC3", normalized: false, min: Some(pmin), max: Some(pmax) })?;

    // 1: NORMAL (f32 VEC3)
    let nrm_off = bin.len();
    for &n in mesh.normals.iter() {
        let len = n.length();
        let u = if len > 1e-12 { n * (1.0 / len) } else { Vec3::new(0.0, 0.0, -1.0) };
        for v in [u.x as f32, u.y as f32, u.z as f32] {
            push_bytes(&mut bin, &v.to_le_bytes())?;
        }
    }
    let nrm_len = bin.len() - nrm_off;
    pad4(&mut bin, 0)?;
    let nrm_bv = bvs.len();
    push(&mut bvs, BvMeta { offset: nrm_off, length: nrm_len, target: Some(34962) })?;
    let nrm_acc = accs.len();
    push(&mut accs, AccMeta { bv: nrm_bv, component_type: 5126, count: n_verts, typ: "VEC3", normalized: false, min: None, max: None })?;

    // 2: TEXCOORD_0 (f32 VEC2)
    let uv_off = bin.len();
    for &uv in mesh.uvs.iter() {
        push_bytes(&mut bin, &(uv.x as f32).to_le_bytes())?;
        push_bytes(&mut bin, &(uv.y as f32).to_le_bytes())?;
    }
    let uv_len = bin.len() - uv_off;
    pad4(&mut bin, 0)?;
    let uv_bv = bvs.len();
    push(&mut bvs, BvMeta { offset: uv_off, length: uv_len, target: Some(34962) })?;
    let uv_acc = accs.len();
    push(&mut accs, AccMeta { bv: uv_bv, component_type: 5126, count: n_verts, typ: "VEC2", normalized: false, min: None, max: None })?;

    // Optional _REST_POS_M (f32 VEC2: physical metric grain-aligned (weft_m, warp_m) independent of TEXCOORD_0!)
    let mut rest_pos_acc = None;
    if has_rest_pos {
        let rp_off = bin.len();
        for &rp in mesh.rest_pos_m.iter() {
            push_bytes(&mut bin, &(rp.x as f32).to_le_bytes())?;
            push_bytes(&mut bin, &(rp.y as f32).to_le_bytes())?;
        }
        let rp_len = bin.len() - rp_off;
        pad4(&mut bin, 0)?;
        let rp_bv = bvs.len();
        push(&mut bvs, BvMeta { offset: rp_off, length: rp_len, target: Some(34962) })?;
        rest_pos_acc = Some(accs.len());
        push(&mut accs, AccMeta { bv: rp_bv, component_type: 5126, count: n_verts, typ: "VEC2", normalized: false, min: None, max: None })?;
    }

    // Optional COLOR_0 (UNSIGNED_BYTE VEC4 normalized: [wear, micro_ao, topstitch, mobility])
    let mut color_acc = None;
    if has_color {
        let c_off = bin.len();
        for &c in mesh.colors.iter() {
            push_bytes(&mut bin, &c)?;
        }
        let c_len = bin.len() - c_off;
        pad4(&mut bin, 0)?;
        let c_bv = bvs.len();
        push(&mut bvs, BvMeta { offset: c_off, length: c_len, target: Some(34962) })?;
        color_acc = Some(accs.len());
        push(&mut accs, AccMeta { bv: c_bv, component_type: 5121, count: n_verts, typ: "VEC4", normalized: true, min: None, max: None })?;
    }

    // Optional Morph Target POSITION delta accessors (f32 VEC3 with exact min/max per glTF 2.0!)
    let mut morph_accs = Vec::new_in(scratch);
    for mt in morph_targets.iter() {
        if mt.position_deltas.len() != n_verts || n_verts == 0 {
            continue;
        }
        let m_off = bin.len();
        let mut mmin = [f64::INFINITY; 3];
        let mut mmax = [f64::NEG_INFINITY; 3];
        for &d in mt.position_deltas.iter() {
            let f = [d.x as f32, d.y as f32, d.z as f32];
            for k in 0..3 {
                let v = f[k] as f64;
                if v < mmin[k] { mmin[k] = v; }
                if v > mmax[k] { mmax[k] = v; }
                push_bytes(&mut bin, &f[k].to_le_bytes())?;
            }
        }
        let m_len = bin.len() - m_off;
        pad4(&mut bin, 0)?;
        let m_bv = bvs.len();
        push(&mut bvs, BvMeta { offset: m_off, length: m_len, target: Some(34962) })?;
        let m_acc = accs.len();
        push(&mut accs, AccMeta { bv: m_bv, component_type: 5126, count: n_verts, typ: "VEC3", normalized: false, min: Some(mmin), max: Some(mmax) })?;
        push(&mut morph_accs, (mt.name, m_acc))?;
    }

    // JOINTS_0 (u16 VEC4) & WEIGHTS_0 (f32 VEC4) + inverseBindMatrices (f32 MAT4)
    let mut joints_acc = None;
    let mut weights_acc = None;
    let mut ibm_acc = None;
    if has_skin {
        let j_off = bin.len();
        for &jv in skin_joints.iter() {
            for idx in jv {
                push_bytes(&mut bin, &idx.to_le_bytes())?;
            }
        }
        let j_len = bin.len() - j_off;
        pad4(&mut bin, 0)?;
        let j_bv = bvs.len();
        push(&mut bvs, BvMeta { offset: j_off, length: j_len, target: Some(34962) })?;
        joints_acc = Some(accs.len());
        push(&mut accs, AccMeta { bv: j_bv, component_type: 5123, count: n_verts, typ: "VEC4", normalized: false, min: None, max: None })?;

        let w_off = bin.len();
        for &wv in skin_weights.iter() {
            for w in wv {
                push_bytes(&mut bin, &w.to_le_bytes())?;
            }
        }
        let w_len = bin.len() - w_off;
        pad4(&mut bin, 0)?;
        let w_bv = bvs.len();
        push(&mut bvs, BvMeta { offset: w_off, length: w_len, target: Some(34962) })?;
        weights_acc = Some(accs.len());
        push(&mut accs, AccMeta { bv: w_bv, component_type: 5126, count: n_verts, typ: "VEC4", normalized: false, min: None, max: None })?;

        let ibm_off = bin.len();
        for j in joints.iter() {
            for &m in j.inverse_bind.iter() {
                push_bytes(&mut bin, &m.to_le_bytes())?;
            }
        }
        let ibm_len = bin.len() - ibm_off;
        pad4(&mut bin, 0)?;
        let ibm_bv = bvs.len();
        push(&mut bvs, BvMeta { offset: ibm_off, length: ibm_len, target: None })?;
        ibm_acc = Some(accs.len());
        push(&mut accs, AccMeta { bv: ibm_bv, component_type: 5126, count: joints.len(), typ: "MAT4", normalized: false, min: None, max: None })?;
    }

    // Group triangles deterministically by fabric material index -> one primitive per active fabric.
    struct PrimMeta {
        indices_acc: usize,
        material_idx: usize,
    }
    let mut prims = Vec::new_in(scratch);
    let mut active_fabrics = Vec::new_in(scratch);
    let mut emitted_tri_panels = Vec::new_in(scratch);
    for (fi, _) in fabrics.iter().enumerate() {
        let mut count = 0usize;
        for &tf in mesh.tri_fabric.iter() {
            if tf as usize == fi {
                count += 1;
            }
        }
        if count == 0 {
            continue;
        }
        let mat_idx = active_fabrics.len();
        push(&mut active_fabrics, fi)?;
        let idx_off = bin.len();
        let use_u32 = n_verts >= 65535;
        for (ti, &[a, b, c]) in mesh.triangles.iter().enumerate() {
            if mesh.tri_fabric[ti] as usize == fi {
                if use_u32 {
                    push_bytes(&mut bin, &a.to_le_bytes())?;
                    push_bytes(&mut bin, &b.to_le_bytes())?;
                    push_bytes(&mut bin, &c.to_le_bytes())?;
                } else {
                    push_bytes(&mut bin, &(a as u16).to_le_bytes())?;
                    push_bytes(&mut bin, &(b as u16).to_le_bytes())?;
                    push_bytes(&mut bin, &(c as u16).to_le_bytes())?;
                }
                if has_panels {
                    push(&mut emitted_tri_panels, mesh.tri_panel[ti])?;
                }
            }
        }
        let idx_len = bin.len() - idx_off;
        pad4(&mut bin, 0)?;
        let bv_i = bvs.len();
        push(&mut bvs, BvMeta { offset: idx_off, length: idx_len, target: Some(34963) })?;
        let acc_i = accs.len();
        push(
            &mut accs,
            AccMeta {
                bv: bv_i,
                component_type: if use_u32 { 5125 } else { 5123 },
                count: count * 3,
                typ: "SCALAR",
                normalized: false,
                min: None,
                max: None,
            },
        )?;
        push(&mut prims, PrimMeta { indices_acc: acc_i, material_idx: mat_idx })?;
    }

    // Optional per-triangle panel ID scalar accessor (UNSIGNED_SHORT SCALAR in primitive-emitted order).
    let mut tri_panel_acc = None;
    if has_panels && !emitted_tri_panels.is_empty() {
        let tp_off = bin.len();
        for &pid in emitted_tri_panels.iter() {
            push_bytes(&mut bin, &pid.to_le_bytes())?;
        }
        let tp_len = bin.len() - tp_off;
        pad4(&mut bin, 0)?;
        let tp_bv = bvs.len();
        push(&mut bvs, BvMeta { offset: tp_off, length: tp_len, target: None })?;
        tri_panel_acc = Some(accs.len());
        push(
            &mut accs,
            AccMeta {
                bv: tp_bv,
                component_type: 5123,
                count: emitted_tri_panels.len(),
                typ: "SCALAR",
                normalized: false,
                min: None,
                max: None,
            },
        )?;
    }

    // Build deterministic JSON chunk.
    let mut j = JsonBuf::new(scratch);
    j.s("{\"asset\":{\"version\":\"2.0\",\"generator\":\"blaze-cad-cloth 1\"}");

    // Scene & Nodes: node 0 = GarmentMesh; nodes 1..=J = skeleton joints.
    j.s(",\"scene\":0,\"scenes\":[{\"nodes\":[0");
    if has_skin {
        for (ji, jt) in joints.iter().enumerate() {
            if jt.parent.is_none() {
                j.s(",");
                j.u((ji + 1) as u64);
            }
        }
    }
    j.s("]}]");

    j.s(",\"nodes\":[{\"name\":\"BlazeGarment\",\"mesh\":0");
    if has_skin {
        j.s(",\"skin\":0");
    }
    j.s("}");
    if has_skin {
        for (ji, jt) in joints.iter().enumerate() {
            j.s(",{\"name\":");
            let name_s = core::str::from_utf8(&jt.name).unwrap_or("joint");
            j.q(name_s);
            let mut has_ch = false;
            for (ci, child) in joints.iter().enumerate() {
                if child.parent == Some(ji as u32) {
                    if !has_ch {
                        j.s(",\"children\":[");
                        has_ch = true;
                    } else {
                        j.s(",");
                    }
                    j.u((ci + 1) as u64);
                }
            }
            if has_ch {
                j.s("]");
            }
            j.s(",\"translation\":[");
            j.f(jt.translation[0]); j.s(","); j.f(jt.translation[1]); j.s(","); j.f(jt.translation[2]);
            j.s("],\"rotation\":[");
            j.f(jt.rotation[0]); j.s(","); j.f(jt.rotation[1]); j.s(","); j.f(jt.rotation[2]); j.s(","); j.f(jt.rotation[3]);
            j.s("],\"scale\":[");
            j.f(jt.scale[0]); j.s(","); j.f(jt.scale[1]); j.s(","); j.f(jt.scale[2]);
            j.s("]}");
        }
    }
    j.s("]");

    if has_skin {
        j.s(",\"skins\":[{\"name\":\"Armature\",\"inverseBindMatrices\":");
        j.u(ibm_acc.unwrap() as u64);
        j.s(",\"joints\":[");
        for ji in 0..joints.len() {
            if ji > 0 { j.s(","); }
            j.u((ji + 1) as u64);
        }
        j.s("]}]");
    }

    // Materials
    j.s(",\"materials\":[");
    for (mi, &fi) in active_fabrics.iter().enumerate() {
        if mi > 0 { j.s(","); }
        let fab = fabrics[fi];
        j.s("{\"name\":");
        j.q(fab.name);
        j.s(",\"doubleSided\":true,\"pbrMetallicRoughness\":{\"baseColorFactor\":[");
        j.f(fab.base_colour[0]); j.s(","); j.f(fab.base_colour[1]); j.s(","); j.f(fab.base_colour[2]); j.s(",1]");
        j.s(",\"metallicFactor\":"); j.f(fab.metallic);
        j.s(",\"roughnessFactor\":"); j.f(fab.roughness);
        j.s("}}");
    }
    j.s("]");

    // Meshes (with optional morph targets + targetNames + blaze_panels extras!)
    j.s(",\"meshes\":[{\"name\":\"BlazeGarmentMesh\"");
    if !morph_accs.is_empty() {
        j.s(",\"weights\":[");
        for k in 0..morph_accs.len() {
            if k > 0 { j.s(","); }
            j.s("0");
        }
        j.s("]");
    }
    if !morph_accs.is_empty() || has_panels {
        j.s(",\"extras\":{");
        let mut need_comma = false;
        if !morph_accs.is_empty() {
            j.s("\"targetNames\":[");
            for (k, &(nm, _)) in morph_accs.iter().enumerate() {
                if k > 0 { j.s(","); }
                j.q(nm);
            }
            j.s("]");
            need_comma = true;
        }
        if has_panels {
            if need_comma {
                j.s(",");
            }
            j.s("\"blaze_panels\":{\"panels\":[");
            for (idx, pm) in mesh.panels.iter().enumerate() {
                if idx > 0 { j.s(","); }
                j.s("{\"id\":");
                j.u(pm.id as u64);
                j.s(",\"name\":");
                j.q(pm.name);
                j.s(",\"kind\":");
                j.q(pm.kind.as_str());
                j.s(",\"layer\":");
                j.u(pm.layer as u64);
                j.s(",\"material\":");
                j.u(pm.fabric_idx as u64);
                if let Some(host) = pm.host_panel {
                    j.s(",\"host_panel\":");
                    j.q(host);
                }
                j.s("}");
            }
            j.s("]");
            if let Some(tpa) = tri_panel_acc {
                j.s(",\"tri_panel_accessor\":");
                j.u(tpa as u64);
            }
            j.s("}");
        }
        j.s("}");
    }
    j.s(",\"primitives\":[");
    for (pi, p) in prims.iter().enumerate() {
        if pi > 0 { j.s(","); }
        j.s("{\"attributes\":{\"POSITION\":");
        j.u(pos_acc as u64);
        j.s(",\"NORMAL\":");
        j.u(nrm_acc as u64);
        j.s(",\"TEXCOORD_0\":");
        j.u(uv_acc as u64);
        if let Some(rpa) = rest_pos_acc {
            j.s(",\"_REST_POS_M\":");
            j.u(rpa as u64);
        }
        if let Some(ca) = color_acc {
            j.s(",\"COLOR_0\":");
            j.u(ca as u64);
        }
        if let (Some(ja), Some(wa)) = (joints_acc, weights_acc) {
            j.s(",\"JOINTS_0\":");
            j.u(ja as u64);
            j.s(",\"WEIGHTS_0\":");
            j.u(wa as u64);
        }
        j.s("},\"indices\":");
        j.u(p.indices_acc as u64);
        j.s(",\"material\":");
        j.u(p.material_idx as u64);
        if !morph_accs.is_empty() {
            j.s(",\"targets\":[");
            for (k, &(_, ma)) in morph_accs.iter().enumerate() {
                if k > 0 { j.s(","); }
                j.s("{\"POSITION\":");
                j.u(ma as u64);
                j.s("}");
            }
            j.s("]");
        }
        j.s("}");
    }
    j.s("]}]");

    // Accessors
    j.s(",\"accessors\":[");
    for (ai, a) in accs.iter().enumerate() {
        if ai > 0 { j.s(","); }
        j.s("{\"bufferView\":"); j.u(a.bv as u64);
        j.s(",\"componentType\":"); j.u(a.component_type);
        if a.normalized {
            j.s(",\"normalized\":true");
        }
        j.s(",\"count\":"); j.u(a.count as u64);
        j.s(",\"type\":"); j.q(a.typ);
        if let (Some(mn), Some(mx)) = (a.min, a.max) {
            j.s(",\"min\":["); j.f(mn[0]); j.s(","); j.f(mn[1]); j.s(","); j.f(mn[2]); j.s("]");
            j.s(",\"max\":["); j.f(mx[0]); j.s(","); j.f(mx[1]); j.s(","); j.f(mx[2]); j.s("]");
        }
        j.s("}");
    }
    j.s("]");

    // BufferViews & Buffers
    j.s(",\"bufferViews\":[");
    for (bi, bv) in bvs.iter().enumerate() {
        if bi > 0 { j.s(","); }
        j.s("{\"buffer\":0,\"byteOffset\":"); j.u(bv.offset as u64);
        j.s(",\"byteLength\":"); j.u(bv.length as u64);
        if let Some(t) = bv.target {
            j.s(",\"target\":"); j.u(t);
        }
        j.s("}");
    }
    j.s("],\"buffers\":[{\"byteLength\":"); j.u(bin.len() as u64); j.s("}]}");

    if let Some(err) = j.failed {
        return Err(GltfIoError::Alloc(err));
    }
    pad4(&mut j.buf, b' ')?;
    pad4(&mut bin, 0)?;

    let total_len = 12 + 8 + j.buf.len() + 8 + bin.len();
    let mut out = Vec::new_in(alloc);
    push_bytes(&mut out, b"glTF")?;
    push_bytes(&mut out, &2u32.to_le_bytes())?;
    push_bytes(&mut out, &(total_len as u32).to_le_bytes())?;
    push_bytes(&mut out, &(j.buf.len() as u32).to_le_bytes())?;
    push_bytes(&mut out, b"JSON")?;
    push_bytes(&mut out, &j.buf)?;
    push_bytes(&mut out, &(bin.len() as u32).to_le_bytes())?;
    push_bytes(&mut out, b"BIN\0")?;
    push_bytes(&mut out, &bin)?;
    Ok(out)
}

/// Write a deterministic skinned binary glTF 2.0 (`.glb`) without morph targets.
pub fn write_skinned_glb<'a>(
    mesh: &GarmentMesh<'_>,
    fabrics: &[Fabric],
    joints: &[JointDef<'_>],
    skin_joints: &[[u16; 4]],
    skin_weights: &[[f32; 4]],
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
) -> Result<Vec<u8, Alloc<'a>>, GltfIoError> {
    write_skinned_glb_with_morphs(mesh, fabrics, joints, skin_joints, skin_weights, &[], alloc, scratch)
}
