//! Minimal dark design tokens for `blaze-shell`.
//!
//! Neutral pitch-dark carbon surfaces (`#09090b`), hairline translucent dividers,
//! high-contrast monochrome typography, and restrained desaturated accents only
//! where semantic distinction earns them.

use blaze_sh::Value;
use blaze_sh::lex::HighlightKind;

/// Convert 8-bit RGB + alpha (`0.0..=1.0`) into `[f32; 4]`.
#[inline]
pub const fn rgba(r: u8, g: u8, b: u8, a: f32) -> [f32; 4] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, a]
}

/// Opaque 8-bit RGB color.
#[inline]
pub const fn rgb(r: u8, g: u8, b: u8) -> [f32; 4] {
    rgba(r, g, b, 1.0)
}

/// Pitch-dark neutral obsidian canvas `#09090b`.
pub const BG_CANVAS: [f32; 4] = rgb(0x09, 0x09, 0x0b);
pub const BG_CANVAS_RGB: [f32; 3] = [0x09 as f32 / 255.0, 0x09 as f32 / 255.0, 0x0b as f32 / 255.0];

/// Seamless top bar (matches canvas).
pub const BG_TOPBAR: [f32; 4] = rgb(0x09, 0x09, 0x0b);
/// Barely-lifted composer / input dock `#0d0d10`.
pub const BG_COMPOSER: [f32; 4] = rgb(0x0d, 0x0d, 0x10);
/// Subtle embedded well (terminal output / code preview) `#0d0d10`.
pub const BG_WELL: [f32; 4] = rgb(0x0d, 0x0d, 0x10);
/// Floating popup surface `#121216`.
pub const BG_POPUP: [f32; 4] = rgb(0x12, 0x12, 0x16);
/// Subtle selection / hover row tint.
pub const BG_SELECTED: [f32; 4] = rgba(0xff, 0xff, 0xff, 0.07);
/// Whisper-quiet chip background (nested expandable pills only).
pub const BG_CHIP: [f32; 4] = rgba(0xff, 0xff, 0xff, 0.045);

/// Ultra-subtle translucent hairline dividers.
pub const HAIRLINE: [f32; 4] = rgba(0xff, 0xff, 0xff, 0.065);
pub const HAIRLINE_FAINT: [f32; 4] = rgba(0xff, 0xff, 0xff, 0.042);
pub const HAIRLINE_STRONG: [f32; 4] = rgba(0xff, 0xff, 0xff, 0.12);

/// Soft drop shadow for floating popups only.
pub const SHADOW_SOFT: [f32; 4] = rgba(0x00, 0x00, 0x00, 0.55);

/// Crisp off-white primary text `#f2f2f5`.
pub const FG_BRIGHT: [f32; 4] = rgb(0xf2, 0xf2, 0xf5);
/// Calm readable body text `#d4d4dc`.
pub const FG_PRIMARY: [f32; 4] = rgb(0xd4, 0xd4, 0xdc);
/// Secondary text (headers, keys, paths, flags) `#7c7c88`.
pub const FG_MUTED: [f32; 4] = rgb(0x7c, 0x7c, 0x88);
/// Whisper-dim metadata (indices, durations, separators) `#4b4b55`.
pub const FG_DIM: [f32; 4] = rgb(0x4b, 0x4b, 0x55);

/// Restrained desaturated accents.
pub const ACCENT_WHITE: [f32; 4] = rgb(0xf4, 0xf4, 0xf7);
pub const ACCENT_ICE: [f32; 4] = rgb(0x9b, 0xba, 0xd8);
pub const ACCENT_SAGE: [f32; 4] = rgb(0x7d, 0xbe, 0x94);
pub const ACCENT_SAND: [f32; 4] = rgb(0xd4, 0xb7, 0x8c);
pub const ACCENT_SEAFOAM: [f32; 4] = rgb(0x88, 0xb8, 0xae);
pub const ACCENT_COPPER: [f32; 4] = rgb(0xcf, 0x97, 0x72);
pub const ACCENT_CORAL: [f32; 4] = rgb(0xdf, 0x6c, 0x75);
pub const ACCENT_CORAL_FAINT: [f32; 4] = rgba(0xdf, 0x6c, 0x75, 0.06);

/// Selection highlight inside the input editor.
pub const BG_SELECTION: [f32; 4] = rgba(0xff, 0xff, 0xff, 0.14);

/// Map a lexer [`HighlightKind`] to a restrained, cohesive syntax palette.
pub fn highlight_color(kind: HighlightKind) -> [f32; 4] {
    match kind {
        HighlightKind::Command => FG_BRIGHT,
        HighlightKind::Keyword => rgb(0xa8, 0xa8, 0xb8),
        HighlightKind::Flag => rgb(0x8c, 0x90, 0x9e),
        HighlightKind::String => ACCENT_SAGE,
        HighlightKind::Number => ACCENT_SAND,
        HighlightKind::Unit => rgb(0xb8, 0x9e, 0x78),
        HighlightKind::Var => ACCENT_ICE,
        HighlightKind::Operator => FG_MUTED,
        HighlightKind::Pipe => rgb(0x64, 0x64, 0x72),
        HighlightKind::Punct => FG_DIM,
        HighlightKind::Comment => FG_DIM,
        HighlightKind::RustCode => ACCENT_COPPER,
        HighlightKind::Error => ACCENT_CORAL,
    }
}

/// Restrained semantic foreground color for a structured [`Value`].
pub fn value_color(val: &Value<'_>) -> [f32; 4] {
    match val {
        Value::Nothing => FG_DIM,
        Value::Bool(true) => ACCENT_SAGE,
        Value::Bool(false) => ACCENT_CORAL,
        Value::Int(_) | Value::Float(_) => ACCENT_SAND,
        Value::String(_) => FG_BRIGHT,
        Value::Size(_) => ACCENT_ICE,
        Value::Duration(_) => ACCENT_SEAFOAM,
        Value::Date(_) => FG_MUTED,
        Value::Range(_) => ACCENT_SAND,
        Value::Binary(_) => FG_MUTED,
        Value::List(_) | Value::Record(_) | Value::Table(_) => ACCENT_ICE,
        Value::Closure(_) | Value::CellPath(_) => FG_MUTED,
        Value::Error(_) => ACCENT_CORAL,
    }
}

/// True when a column / cell of this value type should be right-aligned in tables.
pub fn is_right_aligned(val: &Value<'_>) -> bool {
    matches!(val, Value::Int(_) | Value::Float(_) | Value::Size(_) | Value::Duration(_))
}
