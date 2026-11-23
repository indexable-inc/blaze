//! 2D Tailor's Pattern IR, seam specifications, ease verification (`seam_ease_check`), and
//! deterministic 2D SVG flat-pattern export (`export_pattern_svg`).
//!
//! Every garment panel is authored in true flat 2D metric pattern coordinates `(u, v)` in metres:
//! - `u`: weft direction (horizontal around the body / across the panel).
//! - `v`: warp direction (vertical grainline / down the limb).
//!
//! Each panel is a 4-sided curvilinear Coons patch whose four sides (`Bottom`, `Right`, `Top`,
//! `Left`) may each be composed of one or more named [`SubEdge`] curves (straight segments,
//! quadratic Béziers, or cubic Béziers). Sewn edges are matched 1-to-1 by boundary vertex count
//! during meshing so every seam closes watertightly by construction.

#![blaze::sans_io]

use blaze_math::{Vec2, Vec3, cos, sin, sqrt};
use blaze_own::{Alloc, AllocFailure, Vec};
use blaze_text::{Put, Writes};
use blaze_units::Length;

use crate::fabric::Fabric;
use crate::grain::FabricGrain;

/// Identifies one of the four topological sides of a curvilinear Coons pattern panel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    /// `t = 0`, parameterized left-to-right (`s: 0 -> 1`).
    Bottom,
    /// `s = 1`, parameterized bottom-to-top (`t: 0 -> 1`).
    Right,
    /// `t = 1`, parameterized left-to-right (`s: 0 -> 1`).
    Top,
    /// `s = 0`, parameterized bottom-to-top (`t: 0 -> 1`).
    Left,
}

/// A parametric 2D curve segment in flat pattern coordinates (metres).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Curve2D {
    Line { p0: Vec2, p1: Vec2 },
    Quad { p0: Vec2, p1: Vec2, p2: Vec2 },
    Cubic { p0: Vec2, p1: Vec2, p2: Vec2, p3: Vec2 },
}

impl Curve2D {
    #[inline]
    pub const fn line(p0: [f64; 2], p1: [f64; 2]) -> Self {
        Self::Line { p0: Vec2::new(p0[0], p0[1]), p1: Vec2::new(p1[0], p1[1]) }
    }

    #[inline]
    pub const fn quad(p0: [f64; 2], p1: [f64; 2], p2: [f64; 2]) -> Self {
        Self::Quad { p0: Vec2::new(p0[0], p0[1]), p1: Vec2::new(p1[0], p1[1]), p2: Vec2::new(p2[0], p2[1]) }
    }

    #[inline]
    pub const fn cubic(p0: [f64; 2], p1: [f64; 2], p2: [f64; 2], p3: [f64; 2]) -> Self {
        Self::Cubic {
            p0: Vec2::new(p0[0], p0[1]),
            p1: Vec2::new(p1[0], p1[1]),
            p2: Vec2::new(p2[0], p2[1]),
            p3: Vec2::new(p3[0], p3[1]),
        }
    }

    /// Evaluate the curve at `t in [0, 1]`.
    #[inline]
    pub fn eval(&self, t: f64) -> Vec2 {
        let u = 1.0 - t;
        match *self {
            Self::Line { p0, p1 } => p0 * u + p1 * t,
            Self::Quad { p0, p1, p2 } => p0 * (u * u) + p1 * (2.0 * u * t) + p2 * (t * t),
            Self::Cubic { p0, p1, p2, p3 } => {
                let u2 = u * u;
                let t2 = t * t;
                p0 * (u2 * u) + p1 * (3.0 * u2 * t) + p2 * (3.0 * u * t2) + p3 * (t2 * t)
            }
        }
    }

    /// Start point (`t = 0`).
    #[inline]
    pub const fn start(&self) -> Vec2 {
        match *self {
            Self::Line { p0, .. } | Self::Quad { p0, .. } | Self::Cubic { p0, .. } => p0,
        }
    }

    /// End point (`t = 1`).
    #[inline]
    pub const fn end(&self) -> Vec2 {
        match *self {
            Self::Line { p1, .. } => p1,
            Self::Quad { p2, .. } => p2,
            Self::Cubic { p3, .. } => p3,
        }
    }

    /// Deterministic Simpson / 32-segment chord arc length in metres.
    pub fn arc_length(&self) -> f64 {
        match *self {
            Self::Line { p0, p1 } => (p1 - p0).length(),
            _ => {
                const STEPS: usize = 32;
                let mut total = 0.0;
                let mut prev = self.start();
                for i in 1..=STEPS {
                    let t = (i as f64) / (STEPS as f64);
                    let cur = self.eval(t);
                    total += (cur - prev).length();
                    prev = cur;
                }
                total
            }
        }
    }

    /// Sample `n + 1` points parameterized uniformly by arc length along this curve.
    pub fn eval_arc_fraction(&self, frac: f64) -> Vec2 {
        if matches!(self, Self::Line { .. }) || frac <= 0.0 || frac >= 1.0 {
            return self.eval(frac.clamp(0.0, 1.0));
        }
        const LUT: usize = 32;
        let mut cum = [0.0f64; LUT + 1];
        let mut prev = self.start();
        for i in 1..=LUT {
            let t = (i as f64) / (LUT as f64);
            let cur = self.eval(t);
            cum[i] = cum[i - 1] + (cur - prev).length();
            prev = cur;
        }
        let total = cum[LUT];
        if total <= 1e-12 {
            return self.start();
        }
        let target = frac * total;
        let mut lo = 0usize;
        while lo + 1 < LUT && cum[lo + 1] < target {
            lo += 1;
        }
        let seg = cum[lo + 1] - cum[lo];
        let alpha = if seg > 1e-12 { (target - cum[lo]) / seg } else { 0.0 };
        let t = ((lo as f64) + alpha) / (LUT as f64);
        self.eval(t)
    }
}

/// One named sub-edge along one of a panel's four sides.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SubEdge {
    pub name: &'static str,
    pub curve: Curve2D,
    /// Number of discrete mesh segments along this sub-edge (`>= 1`).
    /// For two edges sewn together by a [`SeamSpec`], both edges must share the same `segments`
    /// count (automatically validated during meshing).
    pub segments: u32,
}

impl SubEdge {
    #[inline]
    pub const fn new(name: &'static str, curve: Curve2D, segments: u32) -> Self {
        Self { name, curve, segments }
    }
}

/// How a flat 2D pattern panel `(u, v)` is initially draped/wrapped into 3D around the mannequin
/// before the sewing and gravity simulation begins.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Wrap3D {
    /// Wrap around a vertical Y-axis elliptical cylinder:
    /// - `u` maps to angle `angle_center + (u - u_center) / radius_ref` around `+Y`
    ///   (with `angle = 0` at `-Z` front, `+pi/2` at `+X` right, `-pi/2` at `-X` left, `pi` at `+Z` back).
    /// - `v` maps directly to world `Y = y_origin + v`.
    VerticalCylinder {
        center_x: f64,
        center_z: f64,
        y_origin: f64,
        radius_x: f64,
        radius_z: f64,
        /// Optional linear taper of `(radius_x, radius_z)` per metre of `v`.
        taper_x: f64,
        taper_z: f64,
        u_center: f64,
        angle_center: f64,
        /// Effective arc-length radius converting `du` to radians (`dtheta = du / arc_radius`).
        arc_radius: f64,
    },
    /// Wrap around an arbitrary limb tube (e.g. left/right arm along `±X`, or tilted leg):
    /// - `v` maps along `axis` from `origin` (`p_axis = origin + axis * v`).
    /// - `u` maps to angle `angle_center + (u - u_center) / arc_radius` around `axis`,
    ///   with `angle = 0` along `ref_radial` and `+pi/2` along `axis x ref_radial`.
    LimbTube {
        origin: Vec3,
        axis: Vec3,
        ref_radial: Vec3,
        radius_a: f64,
        radius_b: f64,
        /// Linear taper of `(radius_a, radius_b)` per metre of `v`.
        taper_a: f64,
        taper_b: f64,
        u_center: f64,
        angle_center: f64,
        arc_radius: f64,
    },
    /// Flat planar placement in 3D: `origin + u_axis * u + v_axis * v`.
    Planar {
        origin: Vec3,
        u_axis: Vec3,
        v_axis: Vec3,
    },
}

impl Wrap3D {
    /// Map a 2D flat-pattern point `(u, v)` to its initial 3D pre-sewing position.
    pub fn map(&self, p: Vec2) -> Vec3 {
        let u = p.x;
        let v = p.y;
        match *self {
            Self::VerticalCylinder {
                center_x,
                center_z,
                y_origin,
                radius_x,
                radius_z,
                taper_x,
                taper_z,
                u_center,
                angle_center,
                arc_radius,
            } => {
                let theta = angle_center + (u - u_center) / arc_radius.max(1e-4);
                let rx = (radius_x + taper_x * v).max(0.01);
                let rz = (radius_z + taper_z * v).max(0.01);
                // glTF Y-up convention matching spacer.glb: front = -Z, right = +X, left = -X, back = +Z.
                let x = center_x + rx * sin(theta);
                let z = center_z - rz * cos(theta);
                let y = y_origin + v;
                Vec3::new(x, y, z)
            }
            Self::LimbTube {
                origin,
                axis,
                ref_radial,
                radius_a,
                radius_b,
                taper_a,
                taper_b,
                u_center,
                angle_center,
                arc_radius,
            } => {
                let ax = axis.normalized().unwrap_or(Vec3::new(1.0, 0.0, 0.0));
                let ra_dir = (ref_radial - ax * ref_radial.dot(ax)).normalized().unwrap_or(Vec3::new(0.0, 1.0, 0.0));
                let rb_dir = ax.cross(ra_dir).normalized().unwrap_or(Vec3::new(0.0, 0.0, 1.0));
                let theta = angle_center + (u - u_center) / arc_radius.max(1e-4);
                let ra = (radius_a + taper_a * v).max(0.008);
                let rb = (radius_b + taper_b * v).max(0.008);
                origin + ax * v + ra_dir * (ra * cos(theta)) + rb_dir * (rb * sin(theta))
            }
            Self::Planar { origin, u_axis, v_axis } => origin + u_axis * u + v_axis * v,
        }
    }
}

/// Pinning rule applied to a named sub-edge during simulation.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PinSpec {
    pub panel: &'static str,
    pub edge: &'static str,
    /// `1.0` = completely fixed to initial 3D position; `(0.0..1.0)` = soft elastic tether
    /// back to initial 3D position (lets fabric settle naturally while preventing slip-off).
    pub stiffness: f64,
    /// Only active during sewing (`true`), or kept through gravity settle (`false`).
    pub sewing_only: bool,
}

/// Elastic/gather rule shrinking or stretching a named edge's rest length (e.g. rib cuff, waistband).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ElasticSpec {
    pub panel: &'static str,
    pub edge: &'static str,
    /// Factor applied to rest edge lengths along this sub-edge (e.g. `0.88` gathers by 12%).
    pub rest_scale: f64,
}

/// A 2D-attached structural pocket, flap, storm placket, or reinforcement patch built cleanly
/// outward along the draped single-sided outer surface normal (zero 2-sided shrinkwrap origami!).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SurfacePatchSpec {
    pub name: &'static str,
    pub panel: &'static str,
    /// Normalized `(s, t)` bounds in `[0, 1] x [0, 1]` on the host panel's Coons grid.
    pub s_min: f64,
    pub s_max: f64,
    pub t_min: f64,
    pub t_max: f64,
    /// Outward bellows extrusion depth in metres (e.g. `0.008..0.016` for cargo pocket).
    pub depth: Length,
    /// Subtle dome bulge at the centre of the pocket/flap for realistic padded volume.
    pub crown: Length,
    /// Bevel taper inset around the perimeter (`0.0..0.35`) for crisp tailored stitched edges.
    pub edge_bevel: f64,
    /// Optional separate angled top flap height fraction (`0.0` = no flap, `0.25..0.38` = pocket flap).
    pub flap_fraction: f64,
    /// Optional corner chamfer at the bottom corners (classic military field pocket bevel).
    pub bottom_chamfer: f64,
    /// Fabric index in `PatternSet::fabrics` for the pocket body.
    pub fabric_idx: usize,
    /// Optional distinct fabric index for the flap (e.g. ballistic Cordura flap on canvas pocket).
    pub flap_fabric_idx: Option<usize>,
    /// Add a real folded central vertical military box-pleat channel down the pocket face.
    pub box_pleat: bool,
}

/// Rigid hardware piece (snap button, zipper pull, belt Cobra buckle, grommet) mounted directly
/// to the draped tangent frame `(T, B, N)` of a host panel at `(s, t)`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum HardwareKind {
    /// Cylindrical bevelled snap button (`radius`, `height`).
    SnapButton { radius: Length, height: Length },
    /// Raised bevelled bar / zipper pull tab (`width`, `length`, `thickness`).
    ZipperPull { width: Length, length: Length, thickness: Length },
    /// Rectangular tactical Cobra buckle plate (`width`, `height`, `thickness`).
    Buckle { width: Length, height: Length, thickness: Length },
    /// Vertical zipper teeth rail along `t` (`width`, `height_t`, `thickness`).
    ZipperRail { width: Length, length_m: f64, thickness: Length },
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct HardwareMount {
    pub name: &'static str,
    pub panel: &'static str,
    pub s: f64,
    pub t: f64,
    pub normal_offset: Length,
    pub kind: HardwareKind,
    pub fabric_idx: usize,
}

/// A 4-sided curvilinear garment pattern panel.
pub struct PanelSpec<'a> {
    pub name: &'static str,
    /// Garment dressing layer (`0` = base undershirt, `1` = trousers, `2` = outer field jacket).
    pub layer: u8,
    pub fabric_idx: usize,
    /// Optional inner lining fabric index used when rim-solidifying near open edges.
    pub lining_fabric_idx: Option<usize>,
    pub bottom: Vec<SubEdge, Alloc<'a>>,
    pub right: Vec<SubEdge, Alloc<'a>>,
    pub top: Vec<SubEdge, Alloc<'a>>,
    pub left: Vec<SubEdge, Alloc<'a>>,
    pub wrap: Wrap3D,
    /// Whether this panel's outer surface normal should be flipped (`false` = `du x dv` points outward).
    pub flip_normal: bool,
    /// Physical fabric grain orientation & independent texture UV transform (default straight grain).
    pub grain: FabricGrain = FabricGrain::STRAIGHT,
}

impl<'a> PanelSpec<'a> {
    /// Total segment count along a side.
    pub fn side_segments(&self, side: Side) -> u32 {
        let edges = self.side_edges(side);
        let mut total = 0u32;
        for e in edges.iter() {
            total += e.segments;
        }
        total
    }

    pub fn side_edges(&self, side: Side) -> &[SubEdge] {
        match side {
            Side::Bottom => &self.bottom,
            Side::Right => &self.right,
            Side::Top => &self.top,
            Side::Left => &self.left,
        }
    }

    /// Find a sub-edge by name and return `(Side, start_seg_index, &SubEdge)`.
    pub fn find_edge(&self, name: &str) -> Option<(Side, u32, &SubEdge)> {
        for side in [Side::Bottom, Side::Right, Side::Top, Side::Left] {
            let mut offset = 0u32;
            for e in self.side_edges(side) {
                if e.name == name {
                    return Some((side, offset, e));
                }
                offset += e.segments;
            }
        }
        None
    }

    /// Evaluate a point on one of the 4 sides at discrete vertex index `k in 0..=side_segments(side)`.
    pub fn eval_side_vertex(&self, side: Side, k: u32) -> Vec2 {
        let edges = self.side_edges(side);
        let mut offset = 0u32;
        for (idx, e) in edges.iter().enumerate() {
            let end = offset + e.segments;
            if k <= end || idx + 1 == edges.len() {
                let local = k.saturating_sub(offset);
                let frac = if e.segments == 0 { 0.0 } else { (local as f64) / (e.segments as f64) };
                return e.curve.eval_arc_fraction(frac.clamp(0.0, 1.0));
            }
            offset = end;
        }
        Vec2::new(0.0, 0.0)
    }
}

/// A seam joining two named sub-edges on two panels (or on the same panel, e.g. cylindrical sleeve).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SeamSpec {
    pub name: &'static str,
    pub panel_a: &'static str,
    pub edge_a: &'static str,
    pub panel_b: &'static str,
    pub edge_b: &'static str,
    /// Reverse vertex traversal order on `edge_b` when matching vertices 1-to-1.
    pub reversed: bool,
    /// Maximum allowed fractional arc-length difference (e.g. `0.15` = 15% sleeve cap ease).
    pub max_ease: f64,
    /// Flat-felled seam trench indentation applied after draping (`0.0` = smooth invisible weld).
    pub topstitch_depth: Length,
    /// Optional rest dihedral angle across this seam in radians (`0.0` = flat, `> 0` = folded crease).
    pub fold_angle_rad: f64,
    /// Whether this seam is topologically welded after sewing (almost always `true`).
    pub weld: bool,
}

impl SeamSpec {
    #[inline]
    pub const fn flat(
        name: &'static str,
        panel_a: &'static str,
        edge_a: &'static str,
        panel_b: &'static str,
        edge_b: &'static str,
    ) -> Self {
        Self {
            name,
            panel_a,
            edge_a,
            panel_b,
            edge_b,
            reversed: false,
            max_ease: 0.12,
            topstitch_depth: Length::nm(800_000),
            fold_angle_rad: 0.0,
            weld: true,
        }
    }
}

/// An internal stitch line attaching one named sub-edge of a secondary panel (e.g. a real
/// simulated hanging pocket flap, folded collar leaf, or belt loop) directly onto an arbitrary
/// interior line `(s0, t0) -> (s1, t1)` of a host panel during physics simulation!
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct InternalSeamSpec {
    pub name: &'static str,
    /// Host panel whose interior surface receives the stitch line.
    pub host_panel: &'static str,
    /// Start `(s0, t0)` and end `(s1, t1)` in normalized `[0, 1]^2` grid coordinates on `host_panel`.
    pub host_s0: f64,
    pub host_t0: f64,
    pub host_s1: f64,
    pub host_t1: f64,
    /// Attached sub-panel name and its stitched boundary edge name.
    pub sub_panel: &'static str,
    pub sub_edge: &'static str,
    /// Reverse traversal order along `sub_edge`.
    pub reversed: bool,
    /// Outward offset from the host surface along host normal (e.g. `2 mm` so flap sits proud).
    pub normal_offset: Length,
    /// Optional target fold angle at the stitch hinge (e.g. `1.45 rad` for a turned-down collar leaf).
    pub fold_angle_rad: f64,
    /// Flat-felled topstitch indentation depth along the stitch line.
    pub topstitch_depth: Length,
}

/// Complete parametric garment pattern collection.
pub struct PatternSet<'a> {
    pub fabrics: Vec<Fabric, Alloc<'a>>,
    pub panels: Vec<PanelSpec<'a>, Alloc<'a>>,
    pub seams: Vec<SeamSpec, Alloc<'a>>,
    pub internal_seams: Vec<InternalSeamSpec, Alloc<'a>>,
    pub pins: Vec<PinSpec, Alloc<'a>>,
    pub elastics: Vec<ElasticSpec, Alloc<'a>>,
    pub patches: Vec<SurfacePatchSpec, Alloc<'a>>,
    pub hardware: Vec<HardwareMount, Alloc<'a>>,
}

impl<'a> PatternSet<'a> {
    pub fn new(alloc: Alloc<'a>) -> Self {
        Self {
            fabrics: Vec::new_in(alloc),
            panels: Vec::new_in(alloc),
            seams: Vec::new_in(alloc),
            internal_seams: Vec::new_in(alloc),
            pins: Vec::new_in(alloc),
            elastics: Vec::new_in(alloc),
            patches: Vec::new_in(alloc),
            hardware: Vec::new_in(alloc),
        }
    }

    pub fn find_panel(&self, name: &str) -> Option<(usize, &PanelSpec<'a>)> {
        self.panels.iter().enumerate().find(|(_, p)| p.name == name)
    }
}

/// Finding reported when [`seam_ease_check`] detects an invalid or mismatched seam.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SeamEaseFinding {
    MissingPanel { seam: &'static str, panel: &'static str },
    MissingEdge { seam: &'static str, panel: &'static str, edge: &'static str },
    SegmentCountMismatch { seam: &'static str, segs_a: u32, segs_b: u32 },
    ExcessiveEase { seam: &'static str, len_a: Length, len_b: Length, ease_ratio: f64, limit: f64 },
}

/// Blaze-style deterministic check verifying every seam's panel/edge existence, exact 1-to-1
/// vertex count match, and tailoring ease ratio (`|len_a - len_b| / min(len_a, len_b)`).
pub fn seam_ease_check(patterns: &PatternSet<'_>) -> Result<(), SeamEaseFinding> {
    for seam in patterns.seams.iter() {
        let (_, pa) = patterns
            .find_panel(seam.panel_a)
            .ok_or(SeamEaseFinding::MissingPanel { seam: seam.name, panel: seam.panel_a })?;
        let (_, pb) = patterns
            .find_panel(seam.panel_b)
            .ok_or(SeamEaseFinding::MissingPanel { seam: seam.name, panel: seam.panel_b })?;
        let (_, _, ea) = pa
            .find_edge(seam.edge_a)
            .ok_or(SeamEaseFinding::MissingEdge { seam: seam.name, panel: seam.panel_a, edge: seam.edge_a })?;
        let (_, _, eb) = pb
            .find_edge(seam.edge_b)
            .ok_or(SeamEaseFinding::MissingEdge { seam: seam.name, panel: seam.panel_b, edge: seam.edge_b })?;
        if ea.segments != eb.segments {
            return Err(SeamEaseFinding::SegmentCountMismatch {
                seam: seam.name,
                segs_a: ea.segments,
                segs_b: eb.segments,
            });
        }
        let la = ea.curve.arc_length();
        let lb = eb.curve.arc_length();
        let denom = la.min(lb).max(1e-6);
        let ease = (la - lb).abs() / denom;
        if ease > seam.max_ease {
            return Err(SeamEaseFinding::ExcessiveEase {
                seam: seam.name,
                len_a: Length::m(la),
                len_b: Length::m(lb),
                ease_ratio: ease,
                limit: seam.max_ease,
            });
        }
    }
    Ok(())
}

struct BytePut<'a> {
    buf: Vec<u8, Alloc<'a>>,
    failed: Option<AllocFailure>,
}

impl<'a> BytePut<'a> {
    fn new(alloc: Alloc<'a>) -> Self {
        Self { buf: Vec::new_in(alloc), failed: None }
    }
    fn push_str(&mut self, s: &str) {
        if self.failed.is_some() {
            return;
        }
        for &b in s.as_bytes() {
            if let Err(e) = self.buf.try_push(b) {
                self.failed = Some(e.why);
                return;
            }
        }
    }
    fn push_f64(&mut self, v: f64) {
        if self.failed.is_some() {
            return;
        }
        let mut sink = PutSink(self);
        let _ = sink.float((v * 100.0).round() / 100.0);
    }
}

struct PutSink<'a, 'b>(&'b mut BytePut<'a>);

impl Put for PutSink<'_, '_> {
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

/// Export a deterministic 2D tailor's SVG blueprint of all flat panels (1 mm = 1 SVG unit) with
/// grainlines, outlines, and panel names so patterns can be visually audited in 2D before draping.
pub fn export_pattern_svg<'a>(patterns: &PatternSet<'_>, alloc: Alloc<'a>) -> Result<Vec<u8, Alloc<'a>>, AllocFailure> {
    let mut out = BytePut::new(alloc);
    let cols = 4usize;
    let cell_w = 550.0f64; // mm
    let cell_h = 950.0f64; // mm
    let rows = (patterns.panels.len() + cols - 1) / cols.max(1);
    let width = (cols as f64) * cell_w + 80.0;
    let height = (rows.max(1) as f64) * cell_h + 80.0;

    out.push_str("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 ");
    out.push_f64(width);
    out.push_str(" ");
    out.push_f64(height);
    out.push_str("\" width=\"");
    out.push_f64(width);
    out.push_str("\" height=\"");
    out.push_f64(height);
    out.push_str("\">\n<rect width=\"100%\" height=\"100%\" fill=\"#faf8f4\"/>\n");

    for (idx, panel) in patterns.panels.iter().enumerate() {
        let col = idx % cols;
        let row = idx / cols;
        let ox = 40.0 + (col as f64) * cell_w + cell_w * 0.5;
        let oy = 40.0 + (row as f64) * cell_h + cell_h * 0.5;

        // Compute 2D bounding box in metres.
        let mut min_u = f64::INFINITY;
        let mut max_u = f64::NEG_INFINITY;
        let mut min_v = f64::INFINITY;
        let mut max_v = f64::NEG_INFINITY;
        for side in [Side::Bottom, Side::Right, Side::Top, Side::Left] {
            let n = panel.side_segments(side).max(1);
            for k in 0..=n {
                let p = panel.eval_side_vertex(side, k);
                min_u = min_u.min(p.x);
                max_u = max_u.max(p.x);
                min_v = min_v.min(p.y);
                max_v = max_v.max(p.y);
            }
        }
        let cu = 0.5 * (min_u + max_u);
        let cv = 0.5 * (min_v + max_v);
        let scale = 900.0; // 1 m -> 900 px (fits comfortably in cell)

        let colour = patterns.fabrics.get(panel.fabric_idx).map(|f| f.base_colour).unwrap_or([0.5, 0.5, 0.5]);
        let r = (colour[0].clamp(0.0, 1.0) * 255.0) as u32;
        let g = (colour[1].clamp(0.0, 1.0) * 255.0) as u32;
        let b = (colour[2].clamp(0.0, 1.0) * 255.0) as u32;

        out.push_str("<path d=\"");
        let mut first = true;
        let emit_pt = |p: Vec2, is_first: &mut bool, o: &mut BytePut<'a>| {
            let x = ox + (p.x - cu) * scale;
            let y = oy - (p.y - cv) * scale;
            o.push_str(if *is_first { "M " } else { "L " });
            *is_first = false;
            o.push_f64(x);
            o.push_str(" ");
            o.push_f64(y);
            o.push_str(" ");
        };
        let nb = panel.side_segments(Side::Bottom);
        for k in 0..=nb {
            emit_pt(panel.eval_side_vertex(Side::Bottom, k), &mut first, &mut out);
        }
        let nr = panel.side_segments(Side::Right);
        for k in 1..=nr {
            emit_pt(panel.eval_side_vertex(Side::Right, k), &mut first, &mut out);
        }
        let nt = panel.side_segments(Side::Top);
        for k in (0..nt).rev() {
            emit_pt(panel.eval_side_vertex(Side::Top, k), &mut first, &mut out);
        }
        let nl = panel.side_segments(Side::Left);
        for k in (0..nl).rev() {
            emit_pt(panel.eval_side_vertex(Side::Left, k), &mut first, &mut out);
        }
        out.push_str("Z\" fill=\"rgb(");
        let mut s = PutSink(&mut out);
        let _ = s.int(r as u64);
        out.push_str(",");
        let mut s = PutSink(&mut out);
        let _ = s.int(g as u64);
        out.push_str(",");
        let mut s = PutSink(&mut out);
        let _ = s.int(b as u64);
        out.push_str(")\" fill-opacity=\"0.22\" stroke=\"#1f242d\" stroke-width=\"1.8\"/>\n");

        // Grainline vertical arrow at panel centre.
        let grain_h = ((max_v - min_v) * 0.35 * scale).max(20.0);
        out.push_str("<line x1=\"");
        out.push_f64(ox);
        out.push_str("\" y1=\"");
        out.push_f64(oy - grain_h * 0.5);
        out.push_str("\" x2=\"");
        out.push_f64(ox);
        out.push_str("\" y2=\"");
        out.push_f64(oy + grain_h * 0.5);
        out.push_str("\" stroke=\"#9c3d28\" stroke-width=\"1.2\" stroke-dasharray=\"5,4\"/>\n");

        // Panel name label.
        out.push_str("<text x=\"");
        out.push_f64(ox);
        out.push_str("\" y=\"");
        out.push_f64(oy + 4.0);
        out.push_str("\" font-family=\"monospace\" font-size=\"15\" text-anchor=\"middle\" fill=\"#111\">");
        out.push_str(panel.name);
        out.push_str("</text>\n");
    }

    out.push_str("</svg>\n");
    if let Some(err) = out.failed {
        return Err(err);
    }
    let _ = sqrt(1.0); // keep import active if needed
    Ok(out.buf)
}
