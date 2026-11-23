//! Instant deterministic studio preview renderer bridging draped garments, mannequin, and boots
//! directly into `blaze-cad-picture::render_mesh_scene` and `blaze-cad-out::png`.
//!
//! Renders real BVH ray-traced studio lighting (3-point lights, Fresnel rim, Fibonacci AO, soft
//! shadows, smooth vertex normals) in pure Rust with zero GPU or Blender dependency so garment
//! silhouettes, folds, seams, pockets, and clearance can be visually inspected in ~1 second!

#![blaze::sans_io]

use blaze_cad_mesh::geom::P3;
use blaze_cad_mesh::{Columns, MaterialRole, MeshView, PartId};
use blaze_cad_out::{Image, Limits, Material, PngKind, PngOpts, Transform, png};
use blaze_cad_picture::{
    MeshNormalSource, MeshPictureOpts, MeshScenePart, PictureError, Projection, RenderMode, Shading, StudioOpts, View, Wireframe,
    render_mesh_scene,
};
use blaze_deflate::Level;
use blaze_math::Vec3;
use blaze_own::{Alloc, AllocFailure, Vec};
use blaze_units::{Angle, Length};

use crate::detail::GarmentMesh;
use crate::fabric::Fabric;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum PreviewError {
    Alloc(AllocFailure),
    MeshShape,
    Picture(PictureError),
    Png,
}

impl From<AllocFailure> for PreviewError {
    fn from(e: AllocFailure) -> Self {
        Self::Alloc(e)
    }
}

#[inline]
fn push<T>(v: &mut Vec<T, Alloc<'_>>, x: T) -> Result<(), PreviewError> {
    v.try_push(x).map_err(|e| PreviewError::Alloc(e.why))
}

/// Camera angle presets for visual garment inspection (glTF Y-up, front = `-Z`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StudioAngle {
    /// Full-body front view (`-Z` looking toward `+Z`).
    FrontFull,
    /// Full-body back view (`+Z` looking toward `-Z`).
    BackFull,
    /// Upper-body 3/4 hero close-up (collar, chest pockets, shoulder drape, sleeves).
    UpperThreeQuarter,
    /// Lower-body cargo trouser & boot-break close-up.
    LowerFront,
}

/// Render a studio PNG preview of the dressed mannequin (`body` + optional `boots` + `garment`)
/// using `blaze-cad-picture::render_mesh_scene` and `blaze-cad-out::png`.
#[allow(clippy::too_many_arguments)]
pub fn render_studio_preview_png<'a>(
    body_pos: &[Vec3],
    body_tris: &[[u32; 3]],
    boots_pos: &[Vec3],
    boots_tris: &[[u32; 3]],
    garment: &GarmentMesh<'_>,
    fabrics: &[Fabric],
    angle: StudioAngle,
    width: u32,
    height: u32,
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
) -> Result<Vec<u8, Alloc<'a>>, PreviewError> {
    // Build combined material table: garment fabrics [0..F], mannequin skin [F], boots [F+1].
    let skin_role = MaterialRole(fabrics.len() as u32);
    let boot_role = MaterialRole((fabrics.len() + 1) as u32);

    let mut materials = Vec::new_in(scratch);
    for fab in fabrics.iter() {
        push(&mut materials, fab.to_out_material())?;
    }
    push(
        &mut materials,
        Material {
            name: "mannequin_skin",
            base_colour: [0.68, 0.56, 0.48, 1.0],
            metallic: 0.0,
            roughness: 0.62,
            emissive: [0.0; 3],
            emissive_strength: None,
            clearcoat: None,
            double_sided: true,
        },
    )?;
    push(
        &mut materials,
        Material {
            name: "mag_boots",
            base_colour: [0.16, 0.14, 0.12, 1.0],
            metallic: 0.15,
            roughness: 0.52,
            emissive: [0.0; 3],
            emissive_strength: None,
            clearcoat: None,
            double_sided: true,
        },
    )?;

    // Convert body mesh into blaze-cad-mesh Columns.
    let (b_p, b_n, b_t, b_parts, b_roles) = to_mesh_columns(body_pos, body_tris, skin_role, PartId(0), scratch)?;
    let (bt_p, bt_n, bt_t, bt_parts, bt_roles) = to_mesh_columns(boots_pos, boots_tris, boot_role, PartId(1), scratch)?;

    // Convert garment mesh with per-triangle fabric roles.
    let mut g_p = Vec::new_in(scratch);
    let mut g_n = Vec::new_in(scratch);
    for i in 0..garment.positions.len() {
        let p = garment.positions[i];
        let n = garment.normals[i];
        push(&mut g_p, [p.x as f32, p.y as f32, p.z as f32])?;
        push(&mut g_n, [n.x as f32, n.y as f32, n.z as f32])?;
    }
    let mut g_parts = Vec::new_in(scratch);
    let mut g_roles = Vec::new_in(scratch);
    for &fi in garment.tri_fabric.iter() {
        push(&mut g_parts, PartId(2))?;
        push(&mut g_roles, MaterialRole(fi as u32))?;
    }

    let mut scene_parts = Vec::new_in(scratch);
    if !b_t.is_empty() {
        let mv = MeshView::new(Columns {
            positions: &b_p,
            normals: &b_n,
            tangents: &[],
            uvs: &[],
            colours: &[],
            triangles: &b_t,
            parts: &b_parts,
            roles: &b_roles,
            creases: &[],
        })
        .map_err(|_| PreviewError::MeshShape)?;
        push(&mut scene_parts, MeshScenePart { mesh: mv, parent: None, transform: Transform::IDENTITY, tint_override: None })?;
    }
    if !bt_t.is_empty() {
        let mv = MeshView::new(Columns {
            positions: &bt_p,
            normals: &bt_n,
            tangents: &[],
            uvs: &[],
            colours: &[],
            triangles: &bt_t,
            parts: &bt_parts,
            roles: &bt_roles,
            creases: &[],
        })
        .map_err(|_| PreviewError::MeshShape)?;
        push(&mut scene_parts, MeshScenePart { mesh: mv, parent: None, transform: Transform::IDENTITY, tint_override: None })?;
    }
    if !garment.triangles.is_empty() {
        let mv = MeshView::new(Columns {
            positions: &g_p,
            normals: &g_n,
            tangents: &[],
            uvs: &[],
            colours: &[],
            triangles: &garment.triangles,
            parts: &g_parts,
            roles: &g_roles,
            creases: &[],
        })
        .map_err(|_| PreviewError::MeshShape)?;
        push(&mut scene_parts, MeshScenePart { mesh: mv, parent: None, transform: Transform::IDENTITY, tint_override: None })?;
    }

    let (eye, target, fov_deg) = match angle {
        StudioAngle::FrontFull => ([0.0, 0.96, -2.65], [0.0, 0.92, 0.0], 38.0),
        StudioAngle::BackFull => ([0.0, 0.96, 2.65], [0.0, 0.92, 0.0], 38.0),
        StudioAngle::UpperThreeQuarter => ([0.52, 1.32, -1.25], [0.0, 1.24, 0.0], 34.0),
        StudioAngle::LowerFront => ([0.22, 0.56, -1.55], [0.0, 0.50, 0.0], 36.0),
    };
    let view = View {
        eye: [Length::m(eye[0]), Length::m(eye[1]), Length::m(eye[2])],
        target: [Length::m(target[0]), Length::m(target[1]), Length::m(target[2])],
        up: [0.0, 1.0, 0.0],
        projection: Projection::Perspective { fov: Angle::degrees(fov_deg) },
        width,
        height,
    };

    let studio = StudioOpts {
        key: 0.90,
        fill: 0.34,
        rim: 0.28,
        ambient: 0.28,
        specular: 0.22,
        shininess: 32.0,
        ao_strength: 0.55,
        ao_steps: 4,
        shadow_strength: 0.45,
        shadow_softness: 10.0,
        shadow_steps: 24,
        srgb: true,
        ..
    };
    let pic = render_mesh_scene(
        &scene_parts,
        &materials,
        view,
        MeshPictureOpts {
            alloc: scratch,
            scratch,
            background: [232, 234, 238, 255],
            shading: Shading::Studio(studio),
            mode: RenderMode::Shaded,
            normal_source: MeshNormalSource::Shading,
            wireframe: Wireframe::Off,
            supersample: 1,
            ..
        },
    )
    .map_err(PreviewError::Picture)?;

    let img = Image { width: pic.width, height: pic.height, kind: PngKind::Rgba8, pixels: &pic.pixels };
    let png_bytes = png(
        img,
        PngOpts { level: Level::Default, limits: Limits::SHIPPED, alloc, scratch },
    )
    .map_err(|_| PreviewError::Png)?;
    Ok(png_bytes)
}

#[allow(clippy::type_complexity)]
fn to_mesh_columns<'a>(
    pos: &[Vec3],
    tris: &[[u32; 3]],
    role: MaterialRole,
    part: PartId,
    alloc: Alloc<'a>,
) -> Result<
    (
        Vec<P3, Alloc<'a>>,
        Vec<P3, Alloc<'a>>,
        Vec<[u32; 3], Alloc<'a>>,
        Vec<PartId, Alloc<'a>>,
        Vec<MaterialRole, Alloc<'a>>,
    ),
    PreviewError,
> {
    let mut p_out = Vec::new_in(alloc);
    let mut n_acc = Vec::new_in(alloc);
    for &p in pos {
        push(&mut p_out, [p.x as f32, p.y as f32, p.z as f32])?;
        push(&mut n_acc, Vec3::ZERO)?;
    }
    let mut t_out = Vec::new_in(alloc);
    let mut parts = Vec::new_in(alloc);
    let mut roles = Vec::new_in(alloc);
    for &[a, b, c] in tris {
        let pa = pos[a as usize];
        let pb = pos[b as usize];
        let pc = pos[c as usize];
        let fnrm = (pb - pa).cross(pc - pa);
        n_acc[a as usize] = n_acc[a as usize] + fnrm;
        n_acc[b as usize] = n_acc[b as usize] + fnrm;
        n_acc[c as usize] = n_acc[c as usize] + fnrm;
        push(&mut t_out, [a, b, c])?;
        push(&mut parts, part)?;
        push(&mut roles, role)?;
    }
    let mut n_out = Vec::new_in(alloc);
    for n in n_acc.iter() {
        let l = n.length();
        let u = if l > 1e-12 { *n * (1.0 / l) } else { Vec3::new(0.0, 1.0, 0.0) };
        push(&mut n_out, [u.x as f32, u.y as f32, u.z as f32])?;
    }
    Ok((p_out, n_out, t_out, parts, roles))
}
