//! Layout & paint styles plus `IsA`-inherited Theme Prefab entities.
//!
//! Every widget entity in `blaze-ui` links `(IsA, style_prefab)` in `blaze_ecs::World`.
//! Updating a single prefab entity immediately cascades to every widget inheriting from it,
//! while individual widgets may override `PaintStyle` or `LayoutStyle` locally.

use core::alloc::Allocator;

use blaze_ecs::{Entity, World};

use crate::error::UiError;
use crate::types::{Edges, Rgba, Theme, Vec2};

/// How an entity sizes itself along an axis.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Sizing {
    /// Shrink-wrap intrinsic content / children.
    #[default]
    Auto,
    /// Exact logical CSS pixel size.
    Fixed(f32),
    /// Proportional share of remaining parent space (`Fill` = `Flex(1.0)`).
    Flex(f32),
}

impl Sizing {
    pub const FILL: Self = Self::Flex(1.0);
}

/// Primary layout axis for a container's `ChildOf` children.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Direction {
    #[default]
    Column,
    Row,
    /// Overlay all children inside the same content box.
    Stack,
}

/// Cross-axis alignment of children inside a container.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    Stretch,
    Start,
    Center,
    End,
}

/// Main-axis justification of children when space remains.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Justify {
    #[default]
    Start,
    Center,
    End,
    SpaceBetween,
}

/// Layout specification component attached to UI entities (or inherited via `IsA`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayoutStyle {
    pub dir: Direction,
    pub width: Sizing,
    pub height: Sizing,
    pub min_w: f32,
    pub max_w: f32,
    pub min_h: f32,
    pub max_h: f32,
    pub padding: Edges,
    pub gap: f32,
    pub align: Align,
    pub justify: Justify,
    /// Clip descendant geometry & glyphs to this entity's rounded rect in the fragment shader.
    pub clip: bool,
    /// Allow vertical wheel scrolling inside this container.
    pub scrollable: bool,
    /// Render in the floating overlay pass (popovers, tooltips, drag previews).
    pub overlay: bool,
    /// Optional absolute position inside parent's rect (for floating panels/tooltips).
    pub abs_pos: Option<Vec2>,
}

impl Default for LayoutStyle {
    fn default() -> Self {
        Self {
            dir: Direction::Column,
            width: Sizing::Auto,
            height: Sizing::Auto,
            min_w: 0.0,
            max_w: 1_000_000.0,
            min_h: 0.0,
            max_h: 1_000_000.0,
            padding: Edges::ZERO,
            gap: 0.0,
            align: Align::Stretch,
            justify: Justify::Start,
            clip: false,
            scrollable: false,
            overlay: false,
            abs_pos: None,
        }
    }
}

impl LayoutStyle {
    #[inline]
    pub const fn row() -> Self {
        Self {
            dir: Direction::Row,
            width: Sizing::Auto,
            height: Sizing::Auto,
            min_w: 0.0,
            max_w: 1_000_000.0,
            min_h: 0.0,
            max_h: 1_000_000.0,
            padding: Edges::ZERO,
            gap: 6.0,
            align: Align::Center,
            justify: Justify::Start,
            clip: false,
            scrollable: false,
            overlay: false,
            abs_pos: None,
        }
    }

    #[inline]
    pub const fn column() -> Self {
        Self {
            dir: Direction::Column,
            width: Sizing::Auto,
            height: Sizing::Auto,
            min_w: 0.0,
            max_w: 1_000_000.0,
            min_h: 0.0,
            max_h: 1_000_000.0,
            padding: Edges::ZERO,
            gap: 6.0,
            align: Align::Stretch,
            justify: Justify::Start,
            clip: false,
            scrollable: false,
            overlay: false,
            abs_pos: None,
        }
    }

    #[inline]
    pub const fn fill(mut self) -> Self {
        self.width = Sizing::FILL;
        self.height = Sizing::FILL;
        self
    }

    #[inline]
    pub const fn width(mut self, w: Sizing) -> Self {
        self.width = w;
        self
    }

    #[inline]
    pub const fn height(mut self, h: Sizing) -> Self {
        self.height = h;
        self
    }

    #[inline]
    pub const fn pad(mut self, padding: Edges) -> Self {
        self.padding = padding;
        self
    }

    #[inline]
    pub const fn gap(mut self, gap: f32) -> Self {
        self.gap = gap;
        self
    }

    #[inline]
    pub const fn align(mut self, align: Align) -> Self {
        self.align = align;
        self
    }

    #[inline]
    pub const fn justify(mut self, justify: Justify) -> Self {
        self.justify = justify;
        self
    }

    #[inline]
    pub const fn clip(mut self, clip: bool) -> Self {
        self.clip = clip;
        self
    }

    #[inline]
    pub const fn scrollable(mut self, scrollable: bool) -> Self {
        self.scrollable = scrollable;
        if scrollable {
            self.clip = true;
        }
        self
    }
}

/// Visual paint specification component (inherited via `IsA` style prefabs and overridable per entity).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaintStyle {
    pub bg: Rgba,
    pub bg_hover: Rgba,
    pub bg_active: Rgba,
    pub bg_selected: Rgba,
    pub border: Rgba,
    pub border_hover: Rgba,
    pub border_focus: Rgba,
    pub border_width: f32,
    pub radius: f32,
    pub shadow_blur: f32,
    pub shadow_offset_y: f32,
    pub shadow_color: Rgba,
    pub text: Rgba,
    pub text_muted: Rgba,
    pub text_size: f32,
}

impl Default for PaintStyle {
    fn default() -> Self {
        Self::transparent(&Theme::obsidian())
    }
}

impl PaintStyle {
    pub const fn transparent(t: &Theme) -> Self {
        Self {
            bg: Rgba::TRANSPARENT,
            bg_hover: Rgba::TRANSPARENT,
            bg_active: Rgba::TRANSPARENT,
            bg_selected: Rgba::TRANSPARENT,
            border: Rgba::TRANSPARENT,
            border_hover: Rgba::TRANSPARENT,
            border_focus: t.fire,
            border_width: 0.0,
            radius: 0.0,
            shadow_blur: 0.0,
            shadow_offset_y: 0.0,
            shadow_color: Rgba::TRANSPARENT,
            text: t.ink1,
            text_muted: t.ink3,
            text_size: t.font_md,
        }
    }

    pub const fn panel(t: &Theme) -> Self {
        Self {
            bg: t.bg2,
            bg_hover: t.bg2,
            bg_active: t.bg2,
            bg_selected: t.bg2,
            border: t.line,
            border_hover: t.line,
            border_focus: t.fire_dim,
            border_width: 1.0,
            radius: t.radius_md,
            shadow_blur: 0.0,
            shadow_offset_y: 0.0,
            shadow_color: Rgba::TRANSPARENT,
            text: t.ink1,
            text_muted: t.ink3,
            text_size: t.font_md,
        }
    }

    pub const fn card(t: &Theme) -> Self {
        Self {
            bg: t.bg3,
            bg_hover: t.hover,
            bg_active: t.bg4,
            bg_selected: t.fire_soft,
            border: t.line,
            border_hover: t.line2,
            border_focus: t.fire,
            border_width: 1.0,
            radius: t.radius_md,
            shadow_blur: 6.0,
            shadow_offset_y: 2.0,
            shadow_color: t.shadow,
            text: t.ink1,
            text_muted: t.ink2,
            text_size: t.font_md,
        }
    }

    pub const fn toolbar(t: &Theme) -> Self {
        Self {
            bg: t.bg2,
            bg_hover: t.bg2,
            bg_active: t.bg2,
            bg_selected: t.bg2,
            border: t.line,
            border_hover: t.line,
            border_focus: t.line,
            border_width: 1.0,
            radius: 0.0,
            shadow_blur: 0.0,
            shadow_offset_y: 0.0,
            shadow_color: Rgba::TRANSPARENT,
            text: t.ink1,
            text_muted: t.ink3,
            text_size: t.font_md,
        }
    }

    pub const fn button(t: &Theme) -> Self {
        Self {
            bg: t.bg3,
            bg_hover: t.bg4,
            bg_active: t.fire_soft,
            bg_selected: t.fire_soft,
            border: t.line,
            border_hover: t.line2,
            border_focus: t.fire,
            border_width: 1.0,
            radius: t.radius_sm,
            shadow_blur: 0.0,
            shadow_offset_y: 0.0,
            shadow_color: Rgba::TRANSPARENT,
            text: t.ink1,
            text_muted: t.ink3,
            text_size: t.font_sm,
        }
    }

    pub const fn button_primary(t: &Theme) -> Self {
        Self {
            bg: t.fire,
            bg_hover: t.fire,
            bg_active: t.fire_dim,
            bg_selected: t.fire,
            border: t.fire,
            border_hover: Rgba::WHITE,
            border_focus: Rgba::WHITE,
            border_width: 1.0,
            radius: t.radius_sm,
            shadow_blur: 0.0,
            shadow_offset_y: 0.0,
            shadow_color: Rgba::TRANSPARENT,
            text: Rgba::WHITE,
            text_muted: Rgba::WHITE,
            text_size: t.font_sm,
        }
    }

    pub const fn input(t: &Theme) -> Self {
        Self {
            bg: t.bg,
            bg_hover: t.bg,
            bg_active: t.bg,
            bg_selected: t.bg,
            border: t.line,
            border_hover: t.line2,
            border_focus: t.fire,
            border_width: 1.0,
            radius: t.radius_sm,
            shadow_blur: 0.0,
            shadow_offset_y: 0.0,
            shadow_color: Rgba::TRANSPARENT,
            text: t.ink1,
            text_muted: t.ink3,
            text_size: t.font_sm,
        }
    }

    pub const fn row(t: &Theme) -> Self {
        Self {
            bg: Rgba::TRANSPARENT,
            bg_hover: t.hover,
            bg_active: t.bg4,
            bg_selected: t.fire_soft,
            border: Rgba::TRANSPARENT,
            border_hover: Rgba::TRANSPARENT,
            border_focus: t.fire_dim,
            border_width: 1.0,
            radius: t.radius_sm,
            shadow_blur: 0.0,
            shadow_offset_y: 0.0,
            shadow_color: Rgba::TRANSPARENT,
            text: t.ink1,
            text_muted: t.ink3,
            text_size: t.font_sm,
        }
    }

    pub const fn tooltip(t: &Theme) -> Self {
        Self {
            bg: t.bg3,
            bg_hover: t.bg3,
            bg_active: t.bg3,
            bg_selected: t.bg3,
            border: t.line2,
            border_hover: t.line2,
            border_focus: t.fire,
            border_width: 1.0,
            radius: t.radius_md,
            shadow_blur: 14.0,
            shadow_offset_y: 5.0,
            shadow_color: t.shadow,
            text: t.ink1,
            text_muted: t.ink2,
            text_size: t.font_sm,
        }
    }
}

/// First-class ECS `Prefab` entities carrying cascading [`PaintStyle`] definitions.
///
/// Widgets link `(IsA, prefabs.button)` (etc.) so updating a single prefab entity restyles every
/// inheriting widget live in O(1) mutations without rebuilding the widget tree.
#[derive(Clone, Copy, Debug)]
pub struct StylePrefabs {
    pub root: Entity,
    pub panel: Entity,
    pub card: Entity,
    pub toolbar: Entity,
    pub button: Entity,
    pub button_primary: Entity,
    pub input: Entity,
    pub row: Entity,
    pub tooltip: Entity,
}

impl StylePrefabs {
    /// Spawn all canonical `Style::*` `Prefab` entities into `world` using `theme`.
    pub fn spawn_in<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        theme: &Theme,
    ) -> Result<Self, UiError> {
        let root = world.spawn_prefab("Style::Root")?;
        let panel = world.spawn_prefab("Style::Panel")?;
        let card = world.spawn_prefab("Style::Card")?;
        let toolbar = world.spawn_prefab("Style::Toolbar")?;
        let button = world.spawn_prefab("Style::Button")?;
        let button_primary = world.spawn_prefab("Style::ButtonPrimary")?;
        let input = world.spawn_prefab("Style::Input")?;
        let row = world.spawn_prefab("Style::Row")?;
        let tooltip = world.spawn_prefab("Style::Tooltip")?;

        let prefabs = Self {
            root,
            panel,
            card,
            toolbar,
            button,
            button_primary,
            input,
            row,
            tooltip,
        };
        prefabs.apply_theme(world, theme)?;
        Ok(prefabs)
    }

    /// Update every prefab entity's [`PaintStyle`] from `theme` (cascades live via `IsA`).
    pub fn apply_theme<A: Allocator + Copy + 'static>(
        &self,
        world: &mut World<A>,
        theme: &Theme,
    ) -> Result<(), UiError> {
        let mut root_style = PaintStyle::transparent(theme);
        root_style.bg = theme.bg;
        world.set_copy(self.root, root_style)?;
        world.set_copy(self.panel, PaintStyle::panel(theme))?;
        world.set_copy(self.card, PaintStyle::card(theme))?;
        world.set_copy(self.toolbar, PaintStyle::toolbar(theme))?;
        world.set_copy(self.button, PaintStyle::button(theme))?;
        world.set_copy(self.button_primary, PaintStyle::button_primary(theme))?;
        world.set_copy(self.input, PaintStyle::input(theme))?;
        world.set_copy(self.row, PaintStyle::row(theme))?;
        world.set_copy(self.tooltip, PaintStyle::tooltip(theme))?;
        Ok(())
    }
}
