//! Geometry, color, and theme palette primitives for `blaze-ui`.

/// Linear/sRGB floating-point RGBA color (`[r, g, b, a]` in `0.0..=1.0`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rgba(pub [f32; 4]);

impl Rgba {
    pub const TRANSPARENT: Self = Self([0.0, 0.0, 0.0, 0.0]);
    pub const WHITE: Self = Self([1.0, 1.0, 1.0, 1.0]);
    pub const BLACK: Self = Self([0.0, 0.0, 0.0, 1.0]);

    #[inline]
    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self([r, g, b, a])
    }

    #[inline]
    pub const fn rgb(r: f32, g: f32, b: f32) -> Self {
        Self([r, g, b, 1.0])
    }

    #[inline]
    pub const fn with_alpha(self, a: f32) -> Self {
        Self([self.0[0], self.0[1], self.0[2], a])
    }

    #[inline]
    pub fn is_transparent(self) -> bool {
        self.0[3] <= 0.001
    }

    #[inline]
    pub fn lerp(self, other: Self, t: f32) -> Self {
        let k = t.clamp(0.0, 1.0);
        let inv = 1.0 - k;
        Self([
            self.0[0] * inv + other.0[0] * k,
            self.0[1] * inv + other.0[1] * k,
            self.0[2] * inv + other.0[2] * k,
            self.0[3] * inv + other.0[3] * k,
        ])
    }
}

/// 2D point or size in logical CSS pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    #[inline]
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// Axis-aligned rectangle in logical CSS pixel coordinates (`(x, y)` = top-left).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0, w: 0.0, h: 0.0 };

    #[inline]
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    #[inline]
    pub fn from_min_max(x0: f32, y0: f32, x1: f32, y1: f32) -> Self {
        Self { x: x0, y: y0, w: (x1 - x0).max(0.0), h: (y1 - y0).max(0.0) }
    }

    #[inline]
    pub fn right(self) -> f32 {
        self.x + self.w
    }

    #[inline]
    pub fn bottom(self) -> f32 {
        self.y + self.h
    }

    #[inline]
    pub fn center(self) -> Vec2 {
        Vec2::new(self.x + self.w * 0.5, self.y + self.h * 0.5)
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    #[inline]
    pub fn contains(self, px: f32, py: f32) -> bool {
        px >= self.x && px <= self.right() && py >= self.y && py <= self.bottom()
    }

    #[inline]
    pub fn inset(self, edges: Edges) -> Self {
        let w = (self.w - edges.left - edges.right).max(0.0);
        let h = (self.h - edges.top - edges.bottom).max(0.0);
        Self { x: self.x + edges.left, y: self.y + edges.top, w, h }
    }

    #[inline]
    pub fn intersect(self, other: Self) -> Self {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = self.right().min(other.right());
        let y1 = self.bottom().min(other.bottom());
        Self::from_min_max(x0, y0, x1, y1)
    }

    #[inline]
    pub fn to_min_max(self) -> [f32; 4] {
        [self.x, self.y, self.right(), self.bottom()]
    }
}

/// Four-sided padding / margin specification in CSS pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Edges {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Edges {
    pub const ZERO: Self = Self { top: 0.0, right: 0.0, bottom: 0.0, left: 0.0 };

    #[inline]
    pub const fn all(v: f32) -> Self {
        Self { top: v, right: v, bottom: v, left: v }
    }

    #[inline]
    pub const fn xy(x: f32, y: f32) -> Self {
        Self { top: y, right: x, bottom: y, left: x }
    }

    #[inline]
    pub const fn new(top: f32, right: f32, bottom: f32, left: f32) -> Self {
        Self { top, right, bottom, left }
    }

    #[inline]
    pub fn horizontal(self) -> f32 {
        self.left + self.right
    }

    #[inline]
    pub fn vertical(self) -> f32 {
        self.top + self.bottom
    }
}

/// Built-in semantic theme presets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeKind {
    /// Blaze signature monochrome obsidian + fire accent (`#101011` / `#e0461a`).
    Obsidian,
    /// Clean daylight paper palette (`#f6f5f2` / `#d94316`).
    Paper,
    /// High-contrast ember terminal palette (`#09090b` / `#ff5a1f`).
    Ember,
}

impl ThemeKind {
    #[inline]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Obsidian => "Obsidian",
            Self::Paper => "Paper",
            Self::Ember => "Ember",
        }
    }
}

/// Complete color and metric design tokens driving all `IsA` style prefabs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub kind: ThemeKind,
    pub bg: Rgba,
    pub bg2: Rgba,
    pub bg3: Rgba,
    pub bg4: Rgba,
    pub hover: Rgba,
    pub line: Rgba,
    pub line2: Rgba,
    pub ink1: Rgba,
    pub ink2: Rgba,
    pub ink3: Rgba,
    pub ink4: Rgba,
    pub fire: Rgba,
    pub fire_soft: Rgba,
    pub fire_dim: Rgba,
    pub ok: Rgba,
    pub ok_soft: Rgba,
    pub warn: Rgba,
    pub warn_soft: Rgba,
    pub danger: Rgba,
    pub danger_soft: Rgba,
    pub blue: Rgba,
    pub blue_soft: Rgba,
    pub emerald: Rgba,
    pub violet: Rgba,
    pub shadow: Rgba,
    pub radius_sm: f32,
    pub radius_md: f32,
    pub radius_lg: f32,
    pub font_sm: f32,
    pub font_md: f32,
    pub font_lg: f32,
}

impl Theme {
    /// Blaze signature monochrome obsidian + fire theme matching `site/styles/tokens.css`.
    pub const fn obsidian() -> Self {
        Self {
            kind: ThemeKind::Obsidian,
            bg: Rgba::rgb(0.063, 0.063, 0.067),
            bg2: Rgba::rgb(0.078, 0.078, 0.084),
            bg3: Rgba::rgb(0.102, 0.102, 0.110),
            bg4: Rgba::rgb(0.137, 0.137, 0.149),
            hover: Rgba::rgb(0.125, 0.125, 0.137),
            line: Rgba::rgb(0.157, 0.157, 0.173),
            line2: Rgba::rgb(0.235, 0.235, 0.255),
            ink1: Rgba::rgb(0.922, 0.918, 0.902),
            ink2: Rgba::rgb(0.710, 0.706, 0.694),
            ink3: Rgba::rgb(0.502, 0.498, 0.490),
            ink4: Rgba::rgb(0.329, 0.329, 0.345),
            fire: Rgba::rgb(0.878, 0.275, 0.102),
            fire_soft: Rgba::new(0.878, 0.275, 0.102, 0.18),
            fire_dim: Rgba::new(0.878, 0.275, 0.102, 0.48),
            ok: Rgba::rgb(0.298, 0.761, 0.478),
            ok_soft: Rgba::new(0.298, 0.761, 0.478, 0.16),
            warn: Rgba::rgb(0.910, 0.643, 0.227),
            warn_soft: Rgba::new(0.910, 0.643, 0.227, 0.16),
            danger: Rgba::rgb(1.0, 0.443, 0.408),
            danger_soft: Rgba::new(1.0, 0.443, 0.408, 0.16),
            blue: Rgba::rgb(0.290, 0.588, 0.941),
            blue_soft: Rgba::new(0.290, 0.588, 0.941, 0.16),
            emerald: Rgba::rgb(0.145, 0.667, 0.478),
            violet: Rgba::rgb(0.678, 0.525, 0.922),
            shadow: Rgba::new(0.0, 0.0, 0.0, 0.46),
            radius_sm: 4.0,
            radius_md: 6.0,
            radius_lg: 10.0,
            font_sm: 11.0,
            font_md: 12.5,
            font_lg: 14.5,
        }
    }

    /// Clean daylight paper theme.
    pub const fn paper() -> Self {
        Self {
            kind: ThemeKind::Paper,
            bg: Rgba::rgb(0.945, 0.941, 0.929),
            bg2: Rgba::rgb(0.976, 0.973, 0.965),
            bg3: Rgba::rgb(0.914, 0.910, 0.894),
            bg4: Rgba::rgb(0.859, 0.855, 0.839),
            hover: Rgba::rgb(0.886, 0.882, 0.867),
            line: Rgba::rgb(0.816, 0.812, 0.796),
            line2: Rgba::rgb(0.690, 0.686, 0.667),
            ink1: Rgba::rgb(0.102, 0.102, 0.114),
            ink2: Rgba::rgb(0.282, 0.282, 0.302),
            ink3: Rgba::rgb(0.451, 0.451, 0.471),
            ink4: Rgba::rgb(0.612, 0.612, 0.631),
            fire: Rgba::rgb(0.831, 0.243, 0.075),
            fire_soft: Rgba::new(0.831, 0.243, 0.075, 0.14),
            fire_dim: Rgba::new(0.831, 0.243, 0.075, 0.42),
            ok: Rgba::rgb(0.145, 0.569, 0.333),
            ok_soft: Rgba::new(0.145, 0.569, 0.333, 0.14),
            warn: Rgba::rgb(0.745, 0.475, 0.078),
            warn_soft: Rgba::new(0.745, 0.475, 0.078, 0.14),
            danger: Rgba::rgb(0.835, 0.220, 0.220),
            danger_soft: Rgba::new(0.835, 0.220, 0.220, 0.14),
            blue: Rgba::rgb(0.153, 0.431, 0.788),
            blue_soft: Rgba::new(0.153, 0.431, 0.788, 0.14),
            emerald: Rgba::rgb(0.094, 0.514, 0.369),
            violet: Rgba::rgb(0.494, 0.337, 0.788),
            shadow: Rgba::new(0.05, 0.05, 0.08, 0.16),
            radius_sm: 4.0,
            radius_md: 6.0,
            radius_lg: 10.0,
            font_sm: 11.0,
            font_md: 12.5,
            font_lg: 14.5,
        }
    }

    /// High-contrast ember terminal theme.
    pub const fn ember() -> Self {
        Self {
            kind: ThemeKind::Ember,
            bg: Rgba::rgb(0.035, 0.035, 0.043),
            bg2: Rgba::rgb(0.055, 0.055, 0.067),
            bg3: Rgba::rgb(0.082, 0.078, 0.094),
            bg4: Rgba::rgb(0.122, 0.114, 0.137),
            hover: Rgba::rgb(0.137, 0.118, 0.125),
            line: Rgba::rgb(0.200, 0.180, 0.200),
            line2: Rgba::rgb(0.329, 0.267, 0.255),
            ink1: Rgba::rgb(0.976, 0.965, 0.945),
            ink2: Rgba::rgb(0.796, 0.769, 0.745),
            ink3: Rgba::rgb(0.569, 0.541, 0.525),
            ink4: Rgba::rgb(0.376, 0.357, 0.361),
            fire: Rgba::rgb(1.0, 0.353, 0.122),
            fire_soft: Rgba::new(1.0, 0.353, 0.122, 0.22),
            fire_dim: Rgba::new(1.0, 0.353, 0.122, 0.58),
            ok: Rgba::rgb(0.314, 0.878, 0.549),
            ok_soft: Rgba::new(0.314, 0.878, 0.549, 0.18),
            warn: Rgba::rgb(0.980, 0.725, 0.235),
            warn_soft: Rgba::new(0.980, 0.725, 0.235, 0.18),
            danger: Rgba::rgb(1.0, 0.392, 0.392),
            danger_soft: Rgba::new(1.0, 0.392, 0.392, 0.18),
            blue: Rgba::rgb(0.361, 0.678, 1.0),
            blue_soft: Rgba::new(0.361, 0.678, 1.0, 0.18),
            emerald: Rgba::rgb(0.180, 0.784, 0.569),
            violet: Rgba::rgb(0.769, 0.596, 1.0),
            shadow: Rgba::new(0.0, 0.0, 0.0, 0.62),
            radius_sm: 3.0,
            radius_md: 5.0,
            radius_lg: 8.0,
            font_sm: 11.0,
            font_md: 12.5,
            font_lg: 14.5,
        }
    }

    /// Return the preset for `kind`.
    #[inline]
    pub const fn from_kind(kind: ThemeKind) -> Self {
        match kind {
            ThemeKind::Obsidian => Self::obsidian(),
            ThemeKind::Paper => Self::paper(),
            ThemeKind::Ember => Self::ember(),
        }
    }
}

impl Default for Theme {
    #[inline]
    fn default() -> Self {
        Self::obsidian()
    }
}
