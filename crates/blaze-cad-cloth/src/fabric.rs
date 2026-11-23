//! Physical fabric and trim material parameters calibrated to real-world woven and knit textiles.
//!
//! All mechanical stiffnesses are expressed in SI units (`g/m²`, `N/m`, `N·m`, metres) so the
//! continuum membrane and isometric dihedral bending solver reproduces realistic garment drape
//! across heavy canvas, military twill, ribbed jersey, ballistic Cordura, and full-grain leather.

#![blaze::sans_io]

use blaze_cad_mesh::MaterialRole;
use blaze_cad_out::Material;
use blaze_units::Length;

/// Physical, mechanical, and appearance properties of a textile or trim material.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Fabric {
    /// Semantic glTF material name (e.g. `"mat_jacket"`, `"mat_trousers"`).
    pub name: &'static str,
    /// Numeric material role index used by `blaze-cad-mesh` / `blaze-cad-picture`.
    pub role: MaterialRole,
    /// Areal mass density in grams per square metre (`g/m²`).
    pub areal_density_g_m2: f64,
    /// Warp (vertical grain, pattern `+v`) membrane tensile stiffness (`N/m`).
    pub warp_stiffness: f64,
    /// Weft (horizontal grain, pattern `+u`) membrane tensile stiffness (`N/m`).
    pub weft_stiffness: f64,
    /// In-plane bias shear stiffness (`N/m`).
    pub shear_stiffness: f64,
    /// Isometric dihedral bending stiffness (`N·m`). Higher values create broader, crisper waves.
    pub bending_stiffness: f64,
    /// Physical sheet thickness (used for rim-aware solidify and contact offset).
    pub thickness: Length,
    /// Surface Coulomb friction coefficient against skin, leather, and other cloth layers.
    pub friction: f64,
    /// Real-world repeat size of 1.0 UV tile (`0.25 m` matches ASTRA's `REST_UV_M`).
    pub uv_meters: Length,
    /// Physically calibrated linear/sRGB base colour for studio previews and glTF PBR factor.
    pub base_colour: [f64; 3],
    /// Physically calibrated PBR roughness (`0.0..=1.0`).
    pub roughness: f64,
    /// Physically calibrated PBR metallic (`0.0..=1.0`).
    pub metallic: f64,
}

impl Fabric {
    /// Mass per square metre in `kg/m²`.
    #[inline]
    pub const fn density_kg_m2(&self) -> f64 {
        self.areal_density_g_m2 * 1e-3
    }

    /// Convert to a `blaze-cad-out::Material` for deterministic glTF and studio previews.
    #[inline]
    pub const fn to_out_material(&self) -> Material<'static> {
        Material {
            name: self.name,
            base_colour: [self.base_colour[0], self.base_colour[1], self.base_colour[2], 1.0],
            metallic: self.metallic,
            roughness: self.roughness,
            emissive: [0.0, 0.0, 0.0],
            emissive_strength: None,
            clearcoat: None,
            double_sided: true,
        }
    }

    /// Heavy waxed cotton canvas (360 g/m²) for the dark charcoal spacefarer field jacket.
    /// Real woven cotton canvas flexural rigidity is ~0.00025..0.00045 N·m (not cardboard!).
    pub const COTTON_CANVAS: Fabric = Fabric {
        name: "mat_jacket",
        role: MaterialRole(0),
        areal_density_g_m2: 360.0,
        warp_stiffness: 1400.0,
        weft_stiffness: 1200.0,
        shear_stiffness: 140.0,
        bending_stiffness: 0.00032,
        thickness: Length::nm(1_800_000),
        friction: 0.42,
        uv_meters: Length::nm(250_000_000),
        base_colour: [0.165, 0.180, 0.210],
        roughness: 0.88,
        metallic: 0.0,
    };

    /// Heavyweight military olive-drab twill (310 g/m²) for relaxed cargo field trousers.
    pub const MILITARY_TWILL: Fabric = Fabric {
        name: "mat_trousers",
        role: MaterialRole(1),
        areal_density_g_m2: 310.0,
        warp_stiffness: 1300.0,
        weft_stiffness: 1100.0,
        shear_stiffness: 125.0,
        bending_stiffness: 0.00022,
        thickness: Length::nm(1_500_000),
        friction: 0.38,
        uv_meters: Length::nm(250_000_000),
        base_colour: [0.220, 0.240, 0.155],
        roughness: 0.86,
        metallic: 0.0,
    };

    /// Warm heather-stone ribbed cotton jersey (215 g/m²) for the crew-neck undershirt.
    pub const RIB_JERSEY: Fabric = Fabric {
        name: "mat_undershirt",
        role: MaterialRole(2),
        areal_density_g_m2: 215.0,
        warp_stiffness: 650.0,
        weft_stiffness: 480.0,
        shear_stiffness: 85.0,
        bending_stiffness: 0.00010,
        thickness: Length::nm(1_100_000),
        friction: 0.48,
        uv_meters: Length::nm(250_000_000),
        base_colour: [0.640, 0.590, 0.520],
        roughness: 0.90,
        metallic: 0.0,
    };

    /// 500D ballistic obsidian Cordura nylon (500 g/m²) for cuffs, storm placket, collar lining,
    /// pocket flaps, and reinforcement yokes.
    pub const BALLISTIC_CORDURA: Fabric = Fabric {
        name: "mat_cordura",
        role: MaterialRole(3),
        areal_density_g_m2: 500.0,
        warp_stiffness: 2000.0,
        weft_stiffness: 1850.0,
        shear_stiffness: 280.0,
        bending_stiffness: 0.00095,
        thickness: Length::nm(2_000_000),
        friction: 0.42,
        uv_meters: Length::nm(250_000_000),
        base_colour: [0.088, 0.094, 0.106],
        roughness: 0.78,
        metallic: 0.0,
    };

    /// Full-grain saddlery leather (900 g/m²) for belt, pocket closure tabs, and cuff straps.
    pub const FULL_GRAIN_LEATHER: Fabric = Fabric {
        name: "mat_leather",
        role: MaterialRole(4),
        areal_density_g_m2: 900.0,
        warp_stiffness: 2400.0,
        weft_stiffness: 2200.0,
        shear_stiffness: 650.0,
        bending_stiffness: 0.065,
        thickness: Length::nm(2_400_000),
        friction: 0.65,
        uv_meters: Length::nm(250_000_000),
        base_colour: [0.220, 0.145, 0.095],
        roughness: 0.58,
        metallic: 0.0,
    };

    /// Brushed matte titanium hardware (snaps, zipper pull, Cobra buckle).
    pub const TITANIUM_HARDWARE: Fabric = Fabric {
        name: "mat_metal",
        role: MaterialRole(5),
        areal_density_g_m2: 4500.0,
        warp_stiffness: 1e6,
        weft_stiffness: 1e6,
        shear_stiffness: 1e6,
        bending_stiffness: 10.0,
        thickness: Length::nm(2_500_000),
        friction: 0.30,
        uv_meters: Length::nm(250_000_000),
        base_colour: [0.380, 0.395, 0.420],
        roughness: 0.34,
        metallic: 0.88,
    };

    /// Dark matte anodized polymer/rubberized trim (zipper tape, drawcord toggles).
    pub const DARK_POLYMER: Fabric = Fabric {
        name: "mat_dark",
        role: MaterialRole(6),
        areal_density_g_m2: 1200.0,
        warp_stiffness: 5e5,
        weft_stiffness: 5e5,
        shear_stiffness: 5e5,
        bending_stiffness: 5.0,
        thickness: Length::nm(2_000_000),
        friction: 0.50,
        uv_meters: Length::nm(250_000_000),
        base_colour: [0.060, 0.065, 0.072],
        roughness: 0.55,
        metallic: 0.15,
    };
}
