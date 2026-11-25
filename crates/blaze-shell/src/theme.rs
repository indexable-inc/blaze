//! Visual design tokens for `blaze-shell`: a crisp dark obsidian palette,
//! syntax highlight colors mapped from `blaze_sh::lex::HighlightKind`, and
//! semantic colors for structured `blaze_sh::Value` types.

use blaze_sh::Value;
use blaze_sh::lex::HighlightKind;

/// Convert 8-bit RGB + alpha (`0.0..=1.0`) into linear-friendly `[f32; 4]`.
#[inline]
pub const fn rgba(r: u8, g: u8, b: u8, a: f32) -> [f32; 4] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, a]
}

/// Opaque 8-bit RGB color.
#[inline]
pub const fn rgb(r: u8, g: u8, b: u8) -> [f32; 4] {
    rgba(r, g, b, 1.0)
}

/// Deep obsidian canvas `#101218`.
pub const BG_CANVAS: [f32; 4] = rgb(0x10, 0x12, 0x18);
pub const BG_CANVAS_RGB: [f32; 3] = [0x10 as f32 / 255.0, 0x12 as f32 / 255.0, 0x18 as f32 / 255.0];

/// Top bar background `#141720`.
pub const BG_TOPBAR: [f32; 4] = rgb(0x14, 0x17, 0x20);
/// Elevated card `#161922`.
pub const BG_CARD: [f32; 4] = rgb(0x16, 0x19, 0x22);
/// Inner container (tables, terminal box, code wells) `#12151d`.
pub const BG_INNER: [f32; 4] = rgb(0x12, 0x15, 0x1d);
/// Header strip / table header `#1d212e`.
pub const BG_HEADER: [f32; 4] = rgb(0x1d, 0x21, 0x2e);
/// Alternating table row tint `#191d29`.
pub const BG_ROW_ALT: [f32; 4] = rgb(0x19, 0x1d, 0x29);
/// Floating popup surface `#1b2030`.
pub const BG_POPUP: [f32; 4] = rgb(0x1b, 0x20, 0x30);
/// Active / selected row or pill surface `#24304a`.
pub const BG_SELECTED: [f32; 4] = rgb(0x24, 0x30, 0x4a);
/// Neutral pill surface `#212636`.
pub const BG_PILL: [f32; 4] = rgb(0x21, 0x26, 0x36);

/// Subtle hairline border `#282e40`.
pub const BORDER_SUBTLE: [f32; 4] = rgb(0x28, 0x2e, 0x40);
/// Stronger border for focused cards / popups `#38415c`.
pub const BORDER_STRONG: [f32; 4] = rgb(0x38, 0x41, 0x5c);

/// Soft drop shadow.
pub const SHADOW_SOFT: [f32; 4] = rgba(0x00, 0x00, 0x00, 0.36);

/// Primary crisp foreground `#e6ebf5`.
pub const FG_PRIMARY: [f32; 4] = rgb(0xe6, 0xeb, 0xf5);
/// Pure highlight white `#f8fafc`.
pub const FG_BRIGHT: [f32; 4] = rgb(0xf8, 0xfa, 0xfc);
/// Muted slate `#8b95ad`.
pub const FG_MUTED: [f32; 4] = rgb(0x8b, 0x95, 0xad);
/// Dimmer gutter / punctuation `#5c667d`.
pub const FG_DIM: [f32; 4] = rgb(0x5c, 0x66, 0x7d);

/// Accent cyan/blue `#5eb8ff`.
pub const ACCENT_CYAN: [f32; 4] = rgb(0x5e, 0xb8, 0xff);
pub const ACCENT_CYAN_BG: [f32; 4] = rgba(0x5e, 0xb8, 0xff, 0.14);
pub const ACCENT_CYAN_BORDER: [f32; 4] = rgba(0x5e, 0xb8, 0xff, 0.48);

/// Emerald `#4ade80`.
pub const ACCENT_EMERALD: [f32; 4] = rgb(0x4a, 0xde, 0x80);
pub const ACCENT_EMERALD_BG: [f32; 4] = rgba(0x4a, 0xde, 0x80, 0.14);
pub const ACCENT_EMERALD_BORDER: [f32; 4] = rgba(0x4a, 0xde, 0x80, 0.40);

/// Amber `#fbbf24`.
pub const ACCENT_AMBER: [f32; 4] = rgb(0xfb, 0xbf, 0x24);
pub const ACCENT_AMBER_BG: [f32; 4] = rgba(0xfb, 0xbf, 0x24, 0.14);
pub const ACCENT_AMBER_BORDER: [f32; 4] = rgba(0xfb, 0xbf, 0x24, 0.42);

/// Rose `#f87171`.
pub const ACCENT_ROSE: [f32; 4] = rgb(0xf8, 0x71, 0x71);
pub const ACCENT_ROSE_BG: [f32; 4] = rgba(0xf8, 0x71, 0x71, 0.11);
pub const ACCENT_ROSE_BORDER: [f32; 4] = rgba(0xf8, 0x71, 0x71, 0.50);

/// Lavender `#c084fc`.
pub const ACCENT_LAVENDER: [f32; 4] = rgb(0xc0, 0x84, 0xfc);
pub const ACCENT_LAVENDER_BG: [f32; 4] = rgba(0xc0, 0x84, 0xfc, 0.14);
pub const ACCENT_LAVENDER_BORDER: [f32; 4] = rgba(0xc0, 0x84, 0xfc, 0.42);

/// Rust orange `#fb923c`.
pub const ACCENT_RUST: [f32; 4] = rgb(0xfb, 0x92, 0x3c);
pub const ACCENT_RUST_BG: [f32; 4] = rgba(0xfb, 0x92, 0x3c, 0.15);
pub const ACCENT_RUST_BORDER: [f32; 4] = rgba(0xfb, 0x92, 0x3c, 0.48);

/// Teal `#2dd4bf` for durations & units.
pub const ACCENT_TEAL: [f32; 4] = rgb(0x2d, 0xd4, 0xbf);

/// Selection highlight inside the input editor.
pub const BG_SELECTION: [f32; 4] = rgba(0x5e, 0xb8, 0xff, 0.28);

/// Map a lexer [`HighlightKind`] to its crisp syntax color.
pub fn highlight_color(kind: HighlightKind) -> [f32; 4] {
    match kind {
        HighlightKind::Keyword => ACCENT_LAVENDER,
        HighlightKind::Command => ACCENT_CYAN,
        HighlightKind::Flag => ACCENT_AMBER,
        HighlightKind::String => ACCENT_EMERALD,
        HighlightKind::Number => ACCENT_RUST,
        HighlightKind::Unit => ACCENT_TEAL,
        HighlightKind::Var => rgb(0x93, 0xc5, 0xfd),
        HighlightKind::Operator => rgb(0xf4, 0x72, 0xb6),
        HighlightKind::Pipe => ACCENT_LAVENDER,
        HighlightKind::Punct => FG_MUTED,
        HighlightKind::Comment => FG_DIM,
        HighlightKind::RustCode => ACCENT_RUST,
        HighlightKind::Error => ACCENT_ROSE,
    }
}

/// Semantic foreground color for a structured [`Value`].
pub fn value_color(val: &Value<'_>) -> [f32; 4] {
    match val {
        Value::Nothing => FG_DIM,
        Value::Bool(true) => ACCENT_EMERALD,
        Value::Bool(false) => ACCENT_ROSE,
        Value::Int(_) | Value::Float(_) => ACCENT_AMBER,
        Value::String(_) => FG_PRIMARY,
        Value::Size(_) => ACCENT_CYAN,
        Value::Duration(_) => ACCENT_TEAL,
        Value::Date(_) => ACCENT_LAVENDER,
        Value::Range(_) => ACCENT_RUST,
        Value::Binary(_) => FG_MUTED,
        Value::List(_) | Value::Record(_) | Value::Table(_) => ACCENT_CYAN,
        Value::Closure(_) | Value::CellPath(_) => ACCENT_LAVENDER,
        Value::Error(_) => ACCENT_ROSE,
    }
}

/// True when a column / cell of this value type should be right-aligned in tables.
pub fn is_right_aligned(val: &Value<'_>) -> bool {
    matches!(val, Value::Int(_) | Value::Float(_) | Value::Size(_) | Value::Duration(_))
}
