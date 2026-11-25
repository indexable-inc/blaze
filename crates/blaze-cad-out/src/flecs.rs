//! Direct Flecs v4.2 prefab script writer (`.flecs`) for CAD assemblies (`Scene`).
//!
//! Emits a deterministic Flecs v4.2 script that drops directly into any Flecs v4.2 world:
//! - Uses `@tree ChildOf` on the root prefab so instantiated child parts keep their canonical
//!   names (`ship_1.hull`, `ship_1.door.socket_hinge`).
//! - Splits immutable shared geometry/collider/material specs (`(OnInstantiate, Inherit)`) from
//!   per-instance overrides (`(OnInstantiate, Copy)` on `cad.Transform`, `cad.MaterialWear`,
//!   `cad.Joint`), following the Flecs v4.2 component-wise design rules.
//! - Maps every `Part` hierarchy, transform (turned to the chosen `Up` convention), mesh/LOD
//!   indices matching `glb()`, `MaterialRole` bindings with optional `cc0_pack` URIs, physics
//!   `Mass`, `Joint`, `Collider`, and named `Socket` child entities.
//!
//! Output is 100% deterministic (rule 25): formatted through `blaze-text` with zero `Global`
//! allocations and no `core::fmt`.

#![blaze::sans_io]

use blaze_int::idx;
use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{Alloc, AllocFailure, Vec};
use blaze_text::{Put, Writes};

use crate::plant::FlecsPlant;
use crate::scene::{Collider, Matrix, Part, Scene, Transform, Up, turn, turn_extent, turn_rotation, turn_tensor};
use crate::sink::Bytes;

/// Options for [`flecs_prefab`].
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct FlecsOpts<'a, 's, 'n> {
    /// Flecs module scope enclosing the prefab (e.g. `"assets"` -> `module assets`).
    /// Pass `None` to emit in the root scope.
    pub module: Option<&'n str> = None,
    /// Name of the emitted Flecs prefab entity (e.g. `"Cabin"`, `"FrigateHull"`).
    pub prefab_name: &'n str,
    /// URI or relative path to the paired `.glb` asset (e.g. `"assets/cabin.glb"`).
    pub asset_uri: &'n str,
    /// Coordinate convention for exported vectors (`Up::Y` matches glTF, `Up::Z` keeps CAD Z-up).
    pub up: Up = Up::Y,
    /// When `true`, emits the canonical `cad` component schema block before the prefab so the
    /// `.flecs` file is completely standalone and self-describing.
    pub include_schema: bool = true,
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

/// Errors returned by [`flecs_prefab`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FlecsError {
    EmptyPrefabName,
    ParentOrder { part: u32, parent: u32 },
    Alloc(AllocFailure),
}

impl From<AllocFailure> for FlecsError {
    fn from(e: AllocFailure) -> FlecsError {
        FlecsError::Alloc(e)
    }
}

/// Emit `scene` as a deterministic Flecs v4.2 `.flecs` prefab script.
pub fn flecs_prefab<'a>(scene: &Scene<'_>, opts: FlecsOpts<'a, '_, '_>) -> Result<Vec<u8, Alloc<'a>>, FlecsError> {
    flecs_prefab_planted(scene, opts, NoPlant)
}

/// [`flecs_prefab`] with a planted bug hook for oracle controls.
pub fn flecs_prefab_planted<'a>(
    scene: &Scene<'_>,
    opts: FlecsOpts<'a, '_, '_>,
    plant: impl Planted<FlecsPlant>,
) -> Result<Vec<u8, Alloc<'a>>, FlecsError> {
    if opts.prefab_name.is_empty() {
        return Err(FlecsError::EmptyPrefabName);
    }

    let n = scene.parts.len();
    for (i, part) in scene.parts.iter().enumerate() {
        if let Some(p) = part.parent
            && idx(p) >= i
        {
            return Err(FlecsError::ParentOrder { part: i as u32, parent: p });
        }
    }

    // Precompute deterministic mesh indices matching `glb.rs` (for each part: main mesh, then
    // LODs, then collider hulls).
    let mut main_mesh_idx: Vec<Option<u32>, Alloc<'_>> = Vec::try_with_capacity_in(n, opts.scratch)?;
    let mut lod_first_idx: Vec<u32, Alloc<'_>> = Vec::try_with_capacity_in(n, opts.scratch)?;
    let mut hull_first_idx: Vec<u32, Alloc<'_>> = Vec::try_with_capacity_in(n, opts.scratch)?;
    let mut next_mesh = 0u32;
    for part in scene.parts {
        let m = if part.mesh.is_some() {
            let id = next_mesh;
            next_mesh += 1;
            Some(id)
        } else {
            None
        };
        main_mesh_idx.try_push(m).map_err(|r| r.why)?;
        lod_first_idx.try_push(next_mesh).map_err(|r| r.why)?;
        next_mesh += part.lods.len() as u32;
        hull_first_idx.try_push(next_mesh).map_err(|r| r.why)?;
        if let Some(Collider::Hulls(hulls)) = part.collider {
            next_mesh += hulls.len() as u32;
        }
    }

    // Build parent -> children CSR adjacency (same deterministic scene-order forest as `glb.rs`).
    let mut count: Vec<u32, Alloc<'_>> = Vec::try_with_capacity_in(n, opts.scratch)?;
    for _ in 0..n {
        count.try_push(0).map_err(|r| r.why)?;
    }
    for part in scene.parts {
        if let Some(p) = part.parent {
            count[idx(p)] += 1;
        }
    }
    let mut start: Vec<u32, Alloc<'_>> = Vec::try_with_capacity_in(n + 1, opts.scratch)?;
    let mut cur = 0u32;
    for &c in count.iter() {
        start.try_push(cur).map_err(|r| r.why)?;
        cur += c;
    }
    start.try_push(cur).map_err(|r| r.why)?;
    let mut fill: Vec<u32, Alloc<'_>> = Vec::try_with_capacity_in(n, opts.scratch)?;
    for i in 0..n {
        fill.try_push(start[i]).map_err(|r| r.why)?;
    }
    let mut kids: Vec<u32, Alloc<'_>> = Vec::try_with_capacity_in(idx(cur), opts.scratch)?;
    for _ in 0..idx(cur) {
        kids.try_push(0).map_err(|r| r.why)?;
    }
    for (i, part) in scene.parts.iter().enumerate() {
        if let Some(p) = part.parent {
            let slot = idx(fill[idx(p)]);
            kids[slot] = i as u32;
            fill[idx(p)] += 1;
        }
    }

    let mut out = Bytes::new(opts.alloc);
    {
        let mut w = Writer { out: &mut out };
        w.line("// Generated by blaze-cad-out (Flecs v4.2 prefab)");
        if opts.include_schema {
            write_schema(&mut w);
        }
        if let Some(module) = opts.module {
            w.s("module ");
            w.ident(module);
            w.line("");
            w.line("");
        }

        // Emit material palette entities so parts can bind via typed relationships.
        if !scene.materials.is_empty() {
            w.s("materials_");
            w.ident(opts.prefab_name);
            w.line(" {");
            for (mi, mat) in scene.materials.iter().enumerate() {
                w.indent(1);
                w.ident(mat.name);
                w.line(" {");
                w.indent(2);
                w.s("cad.MaterialBinding: { role: ");
                w.quoted(mat.name);
                w.s(", slot: ");
                w.u(mi as u64);
                w.s(", cc0_pack: ");
                w.quoted(mat.cc0_pack.unwrap_or(""));
                w.s(", base_r: ");
                w.f(mat.base_colour[0]);
                w.s(", base_g: ");
                w.f(mat.base_colour[1]);
                w.s(", base_b: ");
                w.f(mat.base_colour[2]);
                w.s(", base_a: ");
                w.f(mat.base_colour[3]);
                w.s(", metallic: ");
                w.f(mat.metallic);
                w.s(", roughness: ");
                w.f(mat.roughness);
                w.line(" }");
                w.indent(1);
                w.line("}");
            }
            w.line("}");
            w.line("");
        }

        // Emit the root prefab. On Flecs 4.2, `@tree ChildOf` preserves named instance child paths!
        if !plant.has(FlecsPlant::OmitTreeChildOf) {
            w.line("@tree ChildOf");
        }
        w.s("prefab ");
        w.ident(opts.prefab_name);
        w.line(" {");
        w.indent(1);
        w.s("cad.AssetSource: { uri: ");
        w.quoted(opts.asset_uri);
        w.s(", up: ");
        w.quoted(match opts.up {
            Up::Y => "Y",
            Up::Z => "Z",
        });
        w.line(" }");
        w.indent(1);
        w.line("cad.MaterialWear: { wear: 0.0, grime: 0.0, wetness: 0.0, dust: 0.0 }");

        // Emit root parts and recursively (iteratively via explicit stack — rule 18) their subtrees.
        let mut stack: Vec<(usize, usize, bool), Alloc<'_>> = Vec::new_in(opts.scratch);
        for i in (0..n).rev() {
            if scene.parts[i].parent.is_none() {
                stack.try_push((i, 1, false)).map_err(|r| r.why)?;
            }
        }
        while let Some((part_idx, depth, closing)) = stack.pop() {
            if closing {
                w.indent(depth);
                w.line("}");
                continue;
            }
            let part = &scene.parts[part_idx];
            w.indent(depth);
            w.ident(part.name);
            w.line(" {");

            write_part_components(
                &mut w,
                part,
                depth + 1,
                opts.up,
                opts.prefab_name,
                scene,
                main_mesh_idx[part_idx],
                lod_first_idx[part_idx],
                hull_first_idx[part_idx],
                &plant,
            );

            // Push closing brace before children so children emit inside this scope.
            stack.try_push((part_idx, depth, true)).map_err(|r| r.why)?;
            let k_start = idx(start[part_idx]);
            let k_end = idx(start[part_idx + 1]);
            for &kid in kids[k_start..k_end].iter().rev() {
                stack.try_push((idx(kid), depth + 1, false)).map_err(|r| r.why)?;
            }
        }

        w.line("}");
    }
    out.finish().map_err(FlecsError::Alloc)
}

#[allow(clippy::too_many_arguments)]
fn write_part_components<P: Planted<FlecsPlant>>(
    w: &mut Writer<'_, '_>,
    part: &Part<'_>,
    depth: usize,
    up: Up,
    prefab_name: &str,
    scene: &Scene<'_>,
    main_mesh: Option<u32>,
    lod_first: u32,
    hull_first: u32,
    plant: &P,
) {
    // Transform
    match part.transform {
        Transform::Trs(t) => {
            let tr = turn(t.translation, up);
            let rot = turn_rotation(t.rotation, up);
            let sc = turn_extent(t.scale, up);
            w.indent(depth);
            w.s("cad.Transform: { tx: ");
            w.f(tr[0]);
            w.s(", ty: ");
            w.f(tr[1]);
            w.s(", tz: ");
            w.f(tr[2]);
            w.s(", rx: ");
            w.f(rot[0]);
            w.s(", ry: ");
            w.f(rot[1]);
            w.s(", rz: ");
            w.f(rot[2]);
            w.s(", rw: ");
            w.f(rot[3]);
            w.s(", sx: ");
            w.f(sc[0]);
            w.s(", sy: ");
            w.f(sc[1]);
            w.s(", sz: ");
            w.f(sc[2]);
            w.line(" }");
        }
        Transform::Matrix(m) => {
            let turned = Matrix { linear: turn_tensor(m.linear, up), translation: turn(m.translation, up) };
            let cm = turned.column_major();
            w.indent(depth);
            w.s("cad.MatrixTransform: { m: [");
            for (k, &v) in cm.iter().enumerate() {
                if k > 0 {
                    w.s(", ");
                }
                w.f(v);
            }
            w.line("] }");
        }
    }

    // MeshRef & Material relationship
    if let (Some(mesh), Some(mi)) = (part.mesh, main_mesh) {
        w.indent(depth);
        w.s("cad.MeshRef: { node: ");
        w.quoted(part.name);
        w.s(", mesh_index: ");
        w.u(u64::from(mi));
        w.line(" }");

        // Link primary role to material entity via relationship `(cad.UsesMaterial, ...)`.
        if let Some(first_role) = mesh.roles().first()
            && let Some(mat) = scene.materials.get(idx(first_role.0))
        {
            w.indent(depth);
            w.s("(cad.UsesMaterial, materials_");
            w.ident(prefab_name);
            w.s(".");
            w.ident(mat.name);
            w.line(")");
        }
    }

    // Mass
    if let Some(mass) = part.mass {
        let c = turn(mass.centre, up);
        w.indent(depth);
        w.s("cad.Mass: { kg: ");
        w.f(mass.mass);
        w.s(", cx: ");
        w.f(c[0]);
        w.s(", cy: ");
        w.f(c[1]);
        w.s(", cz: ");
        w.f(c[2]);
        w.line(" }");
    }

    // Joint
    if let Some(joint) = part.joint {
        let ax = turn(joint.axis, up);
        let (has_limits, lo, hi) = match joint.limits {
            Some([l, h]) => (true, l, h),
            None => (false, 0.0, 0.0),
        };
        w.indent(depth);
        w.s("cad.Joint: { name: ");
        w.quoted(joint.name);
        w.s(", kind: ");
        w.quoted(joint.kind.name());
        w.s(", ax: ");
        w.f(ax[0]);
        w.s(", ay: ");
        w.f(ax[1]);
        w.s(", az: ");
        w.f(ax[2]);
        w.s(", lower: ");
        w.f(lo);
        w.s(", upper: ");
        w.f(hi);
        w.s(", has_limits: ");
        w.s(if has_limits { "true" } else { "false" });
        w.line(" }");
    }

    // Collider
    if let Some(collider) = part.collider {
        w.indent(depth);
        match collider {
            Collider::Box { centre, half } => {
                let c = turn(centre, up);
                let h = turn_extent(half, up);
                w.s("cad.ColliderBox: { cx: ");
                w.f(c[0]);
                w.s(", cy: ");
                w.f(c[1]);
                w.s(", cz: ");
                w.f(c[2]);
                w.s(", hx: ");
                w.f(h[0]);
                w.s(", hy: ");
                w.f(h[1]);
                w.s(", hz: ");
                w.f(h[2]);
                w.line(" }");
            }
            Collider::Sphere { centre, radius } => {
                let c = turn(centre, up);
                w.s("cad.ColliderSphere: { cx: ");
                w.f(c[0]);
                w.s(", cy: ");
                w.f(c[1]);
                w.s(", cz: ");
                w.f(c[2]);
                w.s(", radius: ");
                w.f(radius);
                w.line(" }");
            }
            Collider::Hulls(hulls) => {
                w.s("cad.ColliderHulls: { first_mesh: ");
                w.u(u64::from(hull_first));
                w.s(", count: ");
                w.u(hulls.len() as u64);
                w.line(" }");
            }
            Collider::Mesh => {
                w.s("cad.ColliderMesh: { mesh_index: ");
                w.u(u64::from(main_mesh.unwrap_or(0)));
                w.line(" }");
            }
        }
    }

    // LODs as child entities
    for (k, lod) in part.lods.iter().enumerate() {
        w.indent(depth);
        w.s("lod_");
        w.u((k + 1) as u64);
        w.s(" { cad.LodLevel: { level: ");
        w.u((k + 1) as u64);
        w.s(", mesh_index: ");
        w.u(u64::from(lod_first) + k as u64);
        w.s(", error_m: ");
        w.f(lod.error.as_m());
        w.s(", switch_distance_m: ");
        w.f(lod.switch_distance.as_m());
        w.line(" } }");
    }

    // Named attachment sockets as child entities
    if !plant.has(FlecsPlant::SkipSockets) {
        for sock in part.sockets {
            w.indent(depth);
            w.s("socket_");
            w.ident(sock.name);
            w.line(" {");
            w.indent(depth + 1);
            w.s("cad.Socket: { name: ");
            w.quoted(sock.name);
            w.line(" }");
            match sock.transform {
                Transform::Trs(t) => {
                    let tr = turn(t.translation, up);
                    let rot = turn_rotation(t.rotation, up);
                    let sc = turn_extent(t.scale, up);
                    w.indent(depth + 1);
                    w.s("cad.Transform: { tx: ");
                    w.f(tr[0]);
                    w.s(", ty: ");
                    w.f(tr[1]);
                    w.s(", tz: ");
                    w.f(tr[2]);
                    w.s(", rx: ");
                    w.f(rot[0]);
                    w.s(", ry: ");
                    w.f(rot[1]);
                    w.s(", rz: ");
                    w.f(rot[2]);
                    w.s(", rw: ");
                    w.f(rot[3]);
                    w.s(", sx: ");
                    w.f(sc[0]);
                    w.s(", sy: ");
                    w.f(sc[1]);
                    w.s(", sz: ");
                    w.f(sc[2]);
                    w.line(" }");
                }
                Transform::Matrix(m) => {
                    let turned = Matrix { linear: turn_tensor(m.linear, up), translation: turn(m.translation, up) };
                    let cm = turned.column_major();
                    w.indent(depth + 1);
                    w.s("cad.MatrixTransform: { m: [");
                    for (k, &v) in cm.iter().enumerate() {
                        if k > 0 {
                            w.s(", ");
                        }
                        w.f(v);
                    }
                    w.line("] }");
                }
            }
            w.indent(depth);
            w.line("}");
        }
    }
}

fn write_schema(w: &mut Writer<'_, '_>) {
    w.line("cad {");
    w.line("  struct AssetSource { uri: string, up: string }");
    w.line("  AssetSource { (OnInstantiate, Inherit) }");
    w.line("  struct Transform { tx: f64, ty: f64, tz: f64, rx: f64, ry: f64, rz: f64, rw: f64, sx: f64, sy: f64, sz: f64 }");
    w.line("  Transform { (OnInstantiate, Copy) }");
    w.line("  struct MatrixTransform { m: f64[16] }");
    w.line("  MatrixTransform { (OnInstantiate, Copy) }");
    w.line("  struct MeshRef { node: string, mesh_index: u32 }");
    w.line("  MeshRef { (OnInstantiate, Inherit) }");
    w.line("  UsesMaterial { (PairIsTag), (Exclusive) }");
    w.line("  struct MaterialBinding { role: string, slot: u32, cc0_pack: string, base_r: f64, base_g: f64, base_b: f64, base_a: f64, metallic: f64, roughness: f64 }");
    w.line("  MaterialBinding { (OnInstantiate, Inherit) }");
    w.line("  struct MaterialWear { wear: f64, grime: f64, wetness: f64, dust: f64 }");
    w.line("  MaterialWear { (OnInstantiate, Copy) }");
    w.line("  struct Mass { kg: f64, cx: f64, cy: f64, cz: f64 }");
    w.line("  Mass { (OnInstantiate, Inherit) }");
    w.line("  struct Joint { name: string, kind: string, ax: f64, ay: f64, az: f64, lower: f64, upper: f64, has_limits: bool }");
    w.line("  Joint { (OnInstantiate, Copy) }");
    w.line("  struct ColliderBox { cx: f64, cy: f64, cz: f64, hx: f64, hy: f64, hz: f64 }");
    w.line("  ColliderBox { (OnInstantiate, Inherit) }");
    w.line("  struct ColliderSphere { cx: f64, cy: f64, cz: f64, radius: f64 }");
    w.line("  ColliderSphere { (OnInstantiate, Inherit) }");
    w.line("  struct ColliderHulls { first_mesh: u32, count: u32 }");
    w.line("  ColliderHulls { (OnInstantiate, Inherit) }");
    w.line("  struct ColliderMesh { mesh_index: u32 }");
    w.line("  ColliderMesh { (OnInstantiate, Inherit) }");
    w.line("  struct LodLevel { level: u32, mesh_index: u32, error_m: f64, switch_distance_m: f64 }");
    w.line("  LodLevel { (OnInstantiate, Inherit) }");
    w.line("  struct Socket { name: string }");
    w.line("  Socket { (OnInstantiate, Inherit) }");
    w.line("}");
    w.line("");
}

struct Writer<'b, 'a> {
    out: &'b mut Bytes<'a>,
}

impl Writer<'_, '_> {
    fn s(&mut self, text: &str) {
        self.out.put_str(text);
    }

    fn line(&mut self, text: &str) {
        self.s(text);
        self.out.put_byte(b'\n');
    }

    fn indent(&mut self, depth: usize) {
        for _ in 0..depth {
            self.s("  ");
        }
    }

    fn u(&mut self, n: u64) {
        self.out.int(n);
    }

    fn f(&mut self, x: f64) {
        self.out.float(if x == 0.0 { 0.0 } else { x });
    }

    fn quoted(&mut self, text: &str) {
        self.out.put_byte(b'"');
        blaze_text::escape::json(text, &mut *self.out);
        self.out.put_byte(b'"');
    }

    /// Sanitize an arbitrary CAD part/material/socket name into a valid Flecs identifier
    /// (`[A-Za-z_][A-Za-z0-9_]*`).
    fn ident(&mut self, name: &str) {
        let mut first = true;
        for b in name.bytes() {
            let valid = b.is_ascii_alphanumeric() || b == b'_';
            if first {
                first = false;
                if b.is_ascii_digit() || !valid {
                    self.out.put_byte(b'_');
                }
                if b.is_ascii_digit() {
                    self.out.put_byte(b);
                    continue;
                }
            }
            self.out.put_byte(if valid { b } else { b'_' });
        }
        if first {
            self.s("_unnamed");
        }
    }
}
