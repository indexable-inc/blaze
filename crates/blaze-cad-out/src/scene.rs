//! What is exported: a [`Scene`] of [`Part`]s in a hierarchy and the [`Material`] of each role.
//!
//! Units are SI: metres, kilograms, kilogram square metres. A part's [`Transform`] places it in
//! its parent's frame (the scene's, for a root). Every vector here is in the scene's frame as
//! [`Up`] says it is; glTF's frame is Y up, and a Z-up scene is turned on the way out by the
//! exact signed permutation `(x, y, z) → (x, z, −y)` (a quarter turn about X), applied to every
//! position, normal, transform, centre, inertia, axis and box, so no value is rounded.

#![blaze::sans_io]

use blaze_cad_mesh::MeshView;
use blaze_units::Length;

/// Which axis is up in the scene handed in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Up {
    /// Already glTF's frame: nothing is turned.
    Y,
    /// The CAD convention: turned to Y up on the way out.
    Z,
}

/// Where a part sits in its parent's frame, written as a glTF node's `translation`/`rotation`/
/// `scale` when it is one, and as its `matrix` when it is not: a mirrored placement
/// (blaze-cad-field's `Placement` with a negative determinant) has no rotation quaternion, so it
/// is written exactly as the matrix it is rather than refactored into a negative scale.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Transform {
    Trs(Trs),
    Matrix(Matrix),
}

impl Transform {
    pub const IDENTITY: Transform = Transform::Trs(Trs::IDENTITY);

    /// Convert this transform into its equivalent affine [`Matrix`].
    pub fn to_matrix(self) -> Matrix {
        match self {
            Transform::Matrix(m) => m,
            Transform::Trs(t) => {
                let [x, y, z, w] = t.rotation;
                let (xx, yy, zz) = (x * x, y * y, z * z);
                let (xy, xz, yz) = (x * y, x * z, y * z);
                let (wx, wy, wz) = (w * x, w * y, w * z);
                let r = [
                    [1.0 - 2.0 * (yy + zz), 2.0 * (xy - wz), 2.0 * (xz + wy)],
                    [2.0 * (xy + wz), 1.0 - 2.0 * (xx + zz), 2.0 * (yz - wx)],
                    [2.0 * (xz - wy), 2.0 * (yz + wx), 1.0 - 2.0 * (xx + yy)],
                ];
                let s = t.scale;
                let linear = [
                    [r[0][0] * s[0], r[0][1] * s[1], r[0][2] * s[2]],
                    [r[1][0] * s[0], r[1][1] * s[1], r[1][2] * s[2]],
                    [r[2][0] * s[0], r[2][1] * s[1], r[2][2] * s[2]],
                ];
                Matrix { linear, translation: t.translation }
            }
        }
    }
}

/// Translation, then rotation (a unit quaternion `[x, y, z, w]`), then scale, as glTF composes
/// them: `T · R · S`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Trs {
    pub translation: [f64; 3],
    pub rotation: [f64; 4],
    pub scale: [f64; 3],
}

impl Trs {
    pub const IDENTITY: Trs = Trs { translation: [0.0; 3], rotation: [0.0, 0.0, 0.0, 1.0], scale: [1.0; 3] };
}

/// An affine map `p ↦ linear · p + translation`, `linear` by rows; written as glTF's column-major
/// 4×4 `matrix` with the last row `0 0 0 1`. Refused when not finite or singular.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Matrix {
    pub linear: [[f64; 3]; 3],
    pub translation: [f64; 3],
}

impl Matrix {
    pub const IDENTITY: Matrix = Matrix {
        linear: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        translation: [0.0; 3],
    };

    /// The 16 numbers glTF writes, column by column.
    pub fn column_major(&self) -> [f64; 16] {
        let (m, t) = (self.linear, self.translation);
        [
            m[0][0], m[1][0], m[2][0], 0.0, m[0][1], m[1][1], m[2][1], 0.0, m[0][2], m[1][2], m[2][2], 0.0, t[0], t[1], t[2], 1.0,
        ]
    }

    /// The determinant of `linear`, expanded along the first row in that fixed order.
    pub fn det(&self) -> f64 {
        let m = self.linear;
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    }

    /// Compose `self` (parent) with `child`: `p ↦ self(child(p))`.
    pub fn compose(self, child: Matrix) -> Matrix {
        let mut linear = [[0.0; 3]; 3];
        for r in 0..3 {
            for c in 0..3 {
                linear[r][c] = self.linear[r][0] * child.linear[0][c]
                    + self.linear[r][1] * child.linear[1][c]
                    + self.linear[r][2] * child.linear[2][c];
            }
        }
        let mut translation = [0.0; 3];
        for r in 0..3 {
            translation[r] = self.linear[r][0] * child.translation[0]
                + self.linear[r][1] * child.translation[1]
                + self.linear[r][2] * child.translation[2]
                + self.translation[r];
        }
        Matrix { linear, translation }
    }
}

/// A part's mass properties, in the part's own frame: the inertia tensor is about the centre of
/// mass (blaze-cad-mesh's `mass_properties` gives it per unit density).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct MassFacts {
    /// Kilograms.
    pub mass: f64,
    /// The centre of mass, metres.
    pub centre: [f64; 3],
    /// Kilogram square metres, about `centre`, symmetric.
    pub inertia: [[f64; 3]; 3],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JointKind {
    Fixed,
    Revolute,
    Prismatic,
    Ball,
}

impl JointKind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            JointKind::Fixed => "fixed",
            JointKind::Revolute => "revolute",
            JointKind::Prismatic => "prismatic",
            JointKind::Ball => "ball",
        }
    }
}

/// How a part moves against its parent, at the part's origin.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Joint<'a> {
    pub name: &'a str,
    pub kind: JointKind,
    /// The axis of a revolute or prismatic joint, in the part's frame (ignored by the others but
    /// still written).
    pub axis: [f64; 3],
    /// `[lower, upper]`: radians for a revolute joint, metres for a prismatic one.
    pub limits: Option<[f64; 2]>,
}

/// The shape a physics engine collides the part as.
#[derive(Clone, Copy, Debug)]
pub enum Collider<'a> {
    Box { centre: [f64; 3], half: [f64; 3] },
    Sphere { centre: [f64; 3], radius: f64 },
    /// Convex pieces (blaze-cad-mesh's `decompose`), each written as a glTF mesh no node draws.
    Hulls(&'a [MeshView<'a>]),
    /// The part's own mesh.
    Mesh,
}

/// A level of detail below the part's own mesh (blaze-cad-mesh's `lod_chain`).
#[derive(Clone, Copy, Debug)]
pub struct Lod<'a> {
    pub mesh: MeshView<'a>,
    /// The two-sided distance bound to the full mesh (written in metres).
    pub error: Length,
    /// From this distance on from the camera the level may be drawn (written in metres).
    pub switch_distance: Length,
}

/// A named attachment socket on a part (e.g. `door.hinge`, `thruster.port`, `bayonet.mount`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Socket<'a> {
    pub name: &'a str,
    pub transform: Transform,
}

/// One node of the hierarchy.
#[derive(Clone, Copy, Debug)]
pub struct Part<'a> {
    pub name: &'a str,
    /// The index of the parent part, which must come earlier in [`Scene::parts`] (so the
    /// hierarchy is a forest by construction); `None` for a root.
    pub parent: Option<u32>,
    pub transform: Transform,
    /// The part's surface; each triangle's role picks its [`Material`]. `None` for a group.
    pub mesh: Option<MeshView<'a>>,
    pub lods: &'a [Lod<'a>],
    pub mass: Option<MassFacts>,
    pub joint: Option<Joint<'a>>,
    pub collider: Option<Collider<'a>>,
    /// Named attachment sockets in this part's local frame (written to `extras.sockets` and Flecs).
    pub sockets: &'a [Socket<'a>] = &[],
}

/// `KHR_materials_clearcoat`'s two factors.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Clearcoat {
    /// `clearcoatFactor`, 0 to 1.
    pub factor: f64,
    /// `clearcoatRoughnessFactor`, 0 to 1.
    pub roughness: f64,
}

/// `KHR_materials_anisotropy`: brushed/machined metal streak strength (`0..=1`) and rotation (radians).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Anisotropy {
    pub strength: f64,
    pub rotation: f64,
}

/// `KHR_materials_sheen`: cloth/moss/velvet grazing retro-reflection colour and roughness.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Sheen {
    pub colour: [f64; 3],
    pub roughness: f64,
}

/// `KHR_materials_transmission` + `KHR_materials_ior` + `KHR_materials_volume`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Transmission {
    pub factor: f64,
    pub ior: f64 = 1.5,
    pub thickness: f64 = 0.0,
    pub attenuation_colour: [f64; 3] = [1.0, 1.0, 1.0],
    pub attenuation_distance: f64 = 0.0,
}

/// Index into [`Scene::images`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ImageId(pub u32);

/// Raw image bytes or uncompressed RGBA8 pixels embedded into the GLB BIN chunk.
#[derive(Clone, Copy, Debug)]
pub enum ImageData<'a> {
    Png(&'a [u8]),
    Jpeg(&'a [u8]),
    Rgba8 { width: u32, height: u32, pixels: &'a [u8] },
}

/// One named image entry in [`Scene::images`].
#[derive(Clone, Copy, Debug)]
pub struct TextureImage<'a> {
    pub name: &'a str,
    pub data: ImageData<'a>,
}

/// Reference to an image in [`Scene::images`] with optional `KHR_texture_transform` repeat/offset.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct TextureRef {
    pub image: ImageId,
    pub scale: [f64; 2] = [1.0, 1.0],
    pub offset: [f64; 2] = [0.0, 0.0],
}

/// Tangent-space normal map slot (`normalTexture`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct NormalTexture {
    pub image: ImageId,
    pub scale: f64 = 1.0,
    pub uv_scale: [f64; 2] = [1.0, 1.0],
}

/// Ambient occlusion map slot (`occlusionTexture`, glTF R channel).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct OcclusionTexture {
    pub image: ImageId,
    pub strength: f64 = 1.0,
    pub uv_scale: [f64; 2] = [1.0, 1.0],
}

/// Full glTF 2.0 PBR texture bindings referencing [`Scene::images`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PbrTextures {
    pub base_colour: Option<TextureRef>,
    pub metallic_roughness: Option<TextureRef>,
    pub normal: Option<NormalTexture>,
    pub occlusion: Option<OcclusionTexture>,
    pub emissive: Option<TextureRef>,
}

impl PbrTextures {
    pub const NONE: PbrTextures = PbrTextures {
        base_colour: None,
        metallic_roughness: None,
        normal: None,
        occlusion: None,
        emissive: None,
    };

    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.base_colour.is_none()
            && self.metallic_roughness.is_none()
            && self.normal.is_none()
            && self.occlusion.is_none()
            && self.emissive.is_none()
    }
}

/// The look of one role (`paint.hull`, `glass.canopy`, `light.nav`): glTF 2.0 metallic-roughness
/// model, PBR texture slots, CC0 pack URI, and KHR material extensions.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Material<'a> {
    pub name: &'a str,
    /// Linear RGBA, each 0 to 1; an alpha below 1 is written as `alphaMode: BLEND`.
    pub base_colour: [f64; 4],
    pub metallic: f64,
    pub roughness: f64,
    /// Linear RGB, each 0 to 1.
    pub emissive: [f64; 3] = [0.0; 3],
    /// `KHR_materials_emissive_strength`: the emissive colour's multiplier, 0 or more.
    pub emissive_strength: Option<f64> = None,
    pub clearcoat: Option<Clearcoat> = None,
    pub double_sided: bool = false,
    pub anisotropy: Option<Anisotropy> = None,
    pub sheen: Option<Sheen> = None,
    pub transmission: Option<Transmission> = None,
    pub pbr_textures: PbrTextures = PbrTextures::NONE,
    /// Optional CC0 PBR pack URI (e.g. `cc0://blaze/WesternCedar_512`) written to `extras.cc0_pack`
    /// and Flecs `cad.PbrMaterial`.
    pub cc0_pack: Option<&'a str> = None,
}

/// Texture indices bound to one [`Material`]: each index names an image in
/// [`TexturePack::png_images`] (and, on read, in [`crate::LoadedGlb::images`]).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct MaterialTextures {
    /// `pbrMetallicRoughness.baseColorTexture` image index.
    pub base_colour: Option<u32>,
    /// `pbrMetallicRoughness.metallicRoughnessTexture` image index.
    pub metallic_roughness: Option<u32>,
    /// `normalTexture` image index.
    pub normal: Option<u32>,
    /// `normalTexture.scale`: must be finite and non-negative (`1.0` is glTF's default).
    pub normal_scale: f64,
}

impl MaterialTextures {
    /// No textures bound, default normal scale `1.0`.
    pub const NONE: MaterialTextures =
        MaterialTextures { base_colour: None, metallic_roughness: None, normal: None, normal_scale: 1.0 };
}

/// Embedded PNG images and per-material texture bindings handed to [`crate::glb_textured`].
#[derive(Clone, Copy, Debug)]
pub struct TexturePack<'a> {
    /// Encoded PNG files embedded into the GLB BIN chunk.
    pub png_images: &'a [&'a [u8]],
    /// Parallel to [`Scene::materials`]; may be empty or shorter, must not be longer.
    pub material_textures: &'a [MaterialTextures],
}

impl TexturePack<'static> {
    /// No images and no texture bindings.
    pub const EMPTY: TexturePack<'static> = TexturePack { png_images: &[], material_textures: &[] };
}

/// The parts and the material of each role: `MaterialRole(i)` is `materials[i]`.
#[derive(Clone, Copy, Debug)]
pub struct Scene<'a> {
    pub parts: &'a [Part<'a>],
    pub materials: &'a [Material<'a>],
    /// Optional embedded images referenced by [`Material::pbr_textures`] (`ImageId`).
    pub images: &'a [TextureImage<'a>] = &[],
}

/// The frame change to glTF's (module documentation): a vector.
pub(crate) fn turn(v: [f64; 3], up: Up) -> [f64; 3] {
    match up {
        Up::Y => v,
        Up::Z => [v[0], v[2], -v[1]],
    }
}

/// The inverse of [`turn`]: glTF's Y-up frame back to `up`.
pub(crate) fn unturn(v: [f64; 3], up: Up) -> [f64; 3] {
    match up {
        Up::Y => v,
        Up::Z => [v[0], -v[2], v[1]],
    }
}

/// The frame change of a magnitude per axis (a scale, a box's half extents): permuted only.
pub(crate) fn turn_extent(v: [f64; 3], up: Up) -> [f64; 3] {
    match up {
        Up::Y => v,
        Up::Z => [v[0], v[2], v[1]],
    }
}

/// The inverse of [`turn_extent`]: a pure axis swap, so its own inverse.
pub(crate) fn unturn_extent(v: [f64; 3], up: Up) -> [f64; 3] {
    turn_extent(v, up)
}

/// The frame change of a rotation: its vector part turns as a vector does (the change is a
/// proper rotation).
pub(crate) fn turn_rotation(q: [f64; 4], up: Up) -> [f64; 4] {
    let v = turn([q[0], q[1], q[2]], up);
    [v[0], v[1], v[2], q[3]]
}

/// The inverse of [`turn_rotation`]: glTF's Y-up quaternion back to `up`.
pub(crate) fn unturn_rotation(q: [f64; 4], up: Up) -> [f64; 4] {
    let v = unturn([q[0], q[1], q[2]], up);
    [v[0], v[1], v[2], q[3]]
}

/// The frame change of a tensor: `C · M · Cᵀ`, with `C` the signed permutation.
pub(crate) fn turn_tensor(m: [[f64; 3]; 3], up: Up) -> [[f64; 3]; 3] {
    match up {
        Up::Y => m,
        Up::Z => {
            let (from, sign) = ([0usize, 2, 1], [1.0f64, 1.0, -1.0]);
            let mut out = [[0.0; 3]; 3];
            for i in 0..3 {
                for j in 0..3 {
                    out[i][j] = sign[i] * sign[j] * m[from[i]][from[j]];
                }
            }
            out
        }
    }
}

/// The inverse of [`turn_tensor`]: `Cᵀ · M · C`.
pub(crate) fn unturn_tensor(m: [[f64; 3]; 3], up: Up) -> [[f64; 3]; 3] {
    match up {
        Up::Y => m,
        Up::Z => {
            let (from, sign) = ([0usize, 2, 1], [1.0f64, -1.0, 1.0]);
            let mut out = [[0.0; 3]; 3];
            for i in 0..3 {
                for j in 0..3 {
                    out[i][j] = sign[i] * sign[j] * m[from[i]][from[j]];
                }
            }
            out
        }
    }
}
