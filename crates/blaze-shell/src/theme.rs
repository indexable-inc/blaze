//! Design tokens matched to the owner's Ghostty theme (`~/.config/ghostty/themes/custom-dark`).
//!
//! Neutral grey ladder rebased on Ghostty's `background = #1e1e1e` (surfaces a few steps above
//! it, `LINE` equal to `split-divider-color = #2a2a2a`), ink stepping down from Ghostty's
//! `foreground = #d4d4d4`, and colour reserved for meaning, taken from the Ghostty 16-colour
//! palette ([`ANSI_16`]) wherever one fits. `--fire` (`#e0461a`) stays the brand accent.

use blaze_sh::Value;
use blaze_sh::lex::HighlightKind;
use blaze_vt::color::{Colors, DEFAULT_PALETTE, DynamicPalette, DynamicRgb, Rgb as VtRgb};

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

/// `c` with its alpha replaced by `a`.
#[inline]
pub const fn with_alpha(c: [f32; 4], a: f32) -> [f32; 4] {
    [c[0], c[1], c[2], a]
}

/// Opaque `[f32; 4]` of a terminal [`VtRgb`].
#[inline]
pub const fn vt(c: VtRgb) -> [f32; 4] {
    rgb(c.r, c.g, c.b)
}

// =============================================================================
// Ghostty terminal colours (`custom-dark`): the one home for them
// =============================================================================

const fn vrgb(r: u8, g: u8, b: u8) -> VtRgb {
    VtRgb { r, g, b }
}

/// Ghostty `background`: `#1e1e1e`.
pub const TERM_BG: VtRgb = vrgb(0x1e, 0x1e, 0x1e);
/// Ghostty `foreground`: `#d4d4d4`.
pub const TERM_FG: VtRgb = vrgb(0xd4, 0xd4, 0xd4);
/// Ghostty `cursor-color`: `#f5e0dc`.
pub const TERM_CURSOR: VtRgb = vrgb(0xf5, 0xe0, 0xdc);
/// The composer's caret: Ghostty `cursor-color` `#f5e0dc`.
pub const CURSOR: [f32; 4] = vt(TERM_CURSOR);
/// Text under a block caret: Ghostty `cursor-text` `#1e1e1e`.
pub const CURSOR_TEXT: [f32; 4] = vt(TERM_BG);

/// Ghostty `palette = 0..15`.
pub const ANSI_16: [VtRgb; 16] = [
    vrgb(0x00, 0x00, 0x00),
    vrgb(0xf3, 0x8b, 0xa8),
    vrgb(0xa6, 0xe3, 0xa1),
    vrgb(0xf9, 0xe2, 0xaf),
    vrgb(0x89, 0xb4, 0xfa),
    vrgb(0xcb, 0xa6, 0xf7),
    vrgb(0x94, 0xe2, 0xd5),
    vrgb(0xe0, 0xd8, 0xc0),
    vrgb(0x58, 0x5b, 0x70),
    vrgb(0xf3, 0x8b, 0xa8),
    vrgb(0xa6, 0xe3, 0xa1),
    vrgb(0xf9, 0xe2, 0xaf),
    vrgb(0x89, 0xb4, 0xfa),
    vrgb(0xcb, 0xa6, 0xf7),
    vrgb(0x94, 0xe2, 0xd5),
    vrgb(0xfa, 0xf0, 0xc8),
];

/// Give a terminal the Ghostty defaults: background, foreground, cursor and the 16 named
/// palette colours (the 256-colour cube and grey ramp stay xterm's). Call once on a fresh
/// terminal, before any OSC can override them.
pub fn apply_terminal_colors(colors: &mut Colors) {
    let mut palette = DEFAULT_PALETTE;
    palette[..16].copy_from_slice(&ANSI_16);
    colors.palette = DynamicPalette::new(palette);
    colors.background = DynamicRgb::new(TERM_BG);
    colors.foreground = DynamicRgb::new(TERM_FG);
    colors.cursor = DynamicRgb::new(TERM_CURSOR);
}

// =============================================================================
// Neutral grey ladder over Ghostty's `#1e1e1e`
// =============================================================================

/// Canvas: Ghostty `background` `#1e1e1e`.
pub const BG_CANVAS: [f32; 4] = vt(TERM_BG);
pub const BG_CANVAS_RGB: [f32; 3] = [BG_CANVAS[0], BG_CANVAS[1], BG_CANVAS[2]];

/// Seamless top bar (the canvas, `#1e1e1e`).
pub const BG_TOPBAR: [f32; 4] = BG_CANVAS;
/// `#232323` — recessed command box & terminal well (like `$ curl ... | sh`).
pub const BG_SURFACE: [f32; 4] = rgb(0x23, 0x23, 0x23);
/// `#282828` — inline file/folder/code chips (like `target/`, `build.rs`).
pub const BG_CHIP: [f32; 4] = rgb(0x28, 0x28, 0x28);
/// `#2e2e2e` — subtle button & progress track fill.
pub const BG_TRACK: [f32; 4] = rgb(0x2e, 0x2e, 0x2e);
/// Floating popup surface `#252525`.
pub const BG_POPUP: [f32; 4] = rgb(0x25, 0x25, 0x25);
/// Hover / visual-mode highlight: a faint wash of Ghostty `foreground`.
pub const BG_SELECTED: [f32; 4] = rgba(0xd4, 0xd4, 0xd4, 0.06);
/// Text selection: Ghostty `selection-background` `#313244` (drawn under the text).
pub const BG_SELECTION: [f32; 4] = rgb(0x31, 0x32, 0x44);

/// Ghostty `split-divider-color`: `#2a2a2a`.
pub const LINE: [f32; 4] = rgb(0x2a, 0x2a, 0x2a);
/// Subtle separator between blocks (`#252525`).
pub const LINE_FAINT: [f32; 4] = rgb(0x25, 0x25, 0x25);
/// Stronger rule (`#363636`).
pub const LINE_STRONG: [f32; 4] = rgb(0x36, 0x36, 0x36);

/// Soft drop shadow for floating popups only.
pub const SHADOW_SOFT: [f32; 4] = rgba(0x00, 0x00, 0x00, 0.55);

// =============================================================================
// Neutral ink ladder down from Ghostty's `foreground`
// =============================================================================

/// Primary ink: Ghostty `foreground` `#d4d4d4`.
pub const INK_1: [f32; 4] = vt(TERM_FG);
/// `#a8a8a8` — secondary body & values.
pub const INK_2: [f32; 4] = rgb(0xa8, 0xa8, 0xa8);
/// `#858585` — headers, flags, timestamps, keys.
pub const INK_3: [f32; 4] = rgb(0x85, 0x85, 0x85);
/// `#5a5a5a` — `$`, `#`, dim metadata.
pub const INK_4: [f32; 4] = rgb(0x5a, 0x5a, 0x5a);

// =============================================================================
// Semantic colours only (Ghostty palette where one fits)
// =============================================================================

/// `--fire`: `#e0461a` — the brand's fire: the one accent for what matters now.
pub const FIRE: [f32; 4] = rgb(0xe0, 0x46, 0x1a);
pub const FIRE_FAINT: [f32; 4] = rgba(0xe0, 0x46, 0x1a, 0.14);
/// Ghostty green (`palette = 2`): `#a6e3a1`.
pub const OK: [f32; 4] = vt(ANSI_16[2]);
/// Ghostty red (`palette = 1`): `#f38ba8`.
pub const DANGER: [f32; 4] = vt(ANSI_16[1]);
pub const DANGER_FAINT: [f32; 4] = rgba(0xf3, 0x8b, 0xa8, 0.06);

/// `--series-1`: `#d95926` (Rust / crate icon)
pub const SERIES_ORANGE: [f32; 4] = rgb(0xd9, 0x59, 0x26);
/// `--series-2`: `#3987e5` (Folder / symlink icon, matching `target/`)
pub const SERIES_BLUE: [f32; 4] = rgb(0x39, 0x87, 0xe5);
/// `--series-3`: `#199e70` (Config / duration accent)
pub const SERIES_AQUA: [f32; 4] = rgb(0x19, 0x9e, 0x70);

// Starship prompt accents, from the Ghostty palette over the `#1e1e1e` canvas.
/// Directory: Ghostty cyan (`palette = 6`) `#94e2d5`.
pub const PROMPT_DIR: [f32; 4] = vt(ANSI_16[6]);
/// Git branch: Ghostty magenta (`palette = 5`) `#cba6f7`.
pub const PROMPT_BRANCH: [f32; 4] = vt(ANSI_16[5]);
/// Git status `[?!+⇡]`: Ghostty red (`palette = 1`) `#f38ba8`.
pub const PROMPT_STATUS: [f32; 4] = vt(ANSI_16[1]);
/// `❯` after a success: Ghostty green (`palette = 2`) `#a6e3a1`.
pub const PROMPT_OK: [f32; 4] = vt(ANSI_16[2]);
/// Sparkle / indicator: Ghostty blue (`palette = 4`) `#89b4fa`.
pub const PROMPT_SPARK: [f32; 4] = vt(ANSI_16[4]);
/// Fish-style inline ghost autosuggestion text.
pub const GHOST_INK: [f32; 4] = rgb(0x5a, 0x5a, 0x5a);
/// Completion & history matched-character highlight: Ghostty blue `#89b4fa`.
pub const MATCH_HIGHLIGHT: [f32; 4] = vt(ANSI_16[4]);
/// IntelliJ New UI dark-blue selected completion row fill.
pub const COMPLETION_SEL_BG: [f32; 4] = rgb(0x27, 0x3b, 0x5c);

// Semantic command-position syntax colors (distinct builtin vs external binary vs abbreviation).
/// Builtin / `blaze-sh` native command (`ls`, `open`, `where`, `sort-by`): Ghostty blue `#89b4fa`.
pub const CMD_BUILTIN: [f32; 4] = vt(ANSI_16[4]);
/// External `$PATH` binary (`fd`, `rg`, `claude`, `git`, `vim`): Ghostty yellow `#f9e2af`.
pub const CMD_EXTERNAL: [f32; 4] = vt(ANSI_16[3]);
/// Command-position abbreviation (`cl`, `gs`, `ll`, `bc`): Ghostty magenta `#cba6f7`.
pub const CMD_ABBREV: [f32; 4] = vt(ANSI_16[5]);
/// Unresolved/unknown command token: Ghostty red `#f38ba8`.
pub const CMD_UNKNOWN: [f32; 4] = vt(ANSI_16[1]);

/// IntelliJ New UI Dark inspired syntax highlighting:
/// builtins in sky-blue, external `$PATH` commands in warm gold, abbreviations in lavender,
/// keywords in warm IntelliJ orange, flags in cool slate-blue, strings in emerald green.
pub fn highlight_color(kind: HighlightKind) -> [f32; 4] {
    match kind {
        HighlightKind::Command => INK_1,
        HighlightKind::Keyword => rgb(0xcf, 0x8e, 0x6d),
        HighlightKind::Flag => rgb(0x88, 0xa8, 0xcf),
        HighlightKind::String => rgb(0x6a, 0xab, 0x73),
        HighlightKind::Number => rgb(0x2a, 0xac, 0xb8),
        HighlightKind::Unit => rgb(0x56, 0xb6, 0xc2),
        HighlightKind::Var => rgb(0xc7, 0x92, 0xea),
        HighlightKind::Operator => rgb(0x9a, 0xa5, 0xb8),
        HighlightKind::Pipe => rgb(0x7c, 0x87, 0x99),
        HighlightKind::Punct => INK_4,
        HighlightKind::Comment => rgb(0x70, 0x76, 0x80),
        HighlightKind::RustCode => INK_1,
        HighlightKind::Error => DANGER,
    }
}

/// Restrained semantic color for structured [`Value`] cells.
pub fn value_color(val: &Value<'_>) -> [f32; 4] {
    match val {
        Value::Nothing => INK_4,
        Value::Bool(true) => OK,
        Value::Bool(false) => INK_3,
        Value::Int(_) | Value::Float(_) => INK_1,
        Value::String(_) => INK_1,
        Value::Path { .. } => PROMPT_DIR,
        Value::Size(_) => INK_2,
        Value::Duration(_) => INK_2,
        Value::Date(_) => INK_3,
        Value::Range(_) => INK_2,
        Value::Binary(_) => INK_3,
        Value::Output(_) => INK_1,
        Value::List(_) | Value::Record(_) | Value::Table(_) => INK_2,
        Value::Closure(_) | Value::CellPath(_) => INK_3,
        Value::Error(_) => DANGER,
    }
}

/// True when a column / cell of this value type should be right-aligned in tables.
pub fn is_right_aligned(val: &Value<'_>) -> bool {
    matches!(val, Value::Int(_) | Value::Float(_) | Value::Size(_) | Value::Duration(_))
}

fn lerp_rgba(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    let t = t.clamp(0.0, 1.0);
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3] + (b[3] - a[3]) * t,
    ]
}

/// Smooth $\log_2$-scale thermal recency heatmap color for timestamps (`modified` / `ColDisplay::HumanTime`):
/// - `just now` / minutes (`heat ~ 0.85..1.0`): ember coral-red (`#F26457`)
/// - hours (`heat ~ 0.55..0.85`, e.g. `5h ago`): warm blaze orange (`#E88B48`)
/// - days (`heat ~ 0.30..0.55`, e.g. `3d ago`): warm amber-gold (`#D9AF62`)
/// - weeks (`heat ~ 0.15..0.30`, e.g. `13d ago`): warm stone (`#9C978E`)
/// - months / years (`heat < 0.15`): cool muted slate (`#666A74`)
pub fn recency_heat_color(age_secs: u64) -> [f32; 4] {
    let h = blaze_dust::recency_heat(age_secs);
    let cold = rgb(0x66, 0x6a, 0x74);
    let stone = rgb(0x9c, 0x97, 0x8e);
    let amber = rgb(0xd9, 0xaf, 0x62);
    let orange = rgb(0xe8, 0x8b, 0x48);
    let hot = rgb(0xf2, 0x64, 0x57);

    if h < 0.22 {
        lerp_rgba(cold, stone, h / 0.22)
    } else if h < 0.45 {
        lerp_rgba(stone, amber, (h - 0.22) / 0.23)
    } else if h < 0.72 {
        lerp_rgba(amber, orange, (h - 0.45) / 0.27)
    } else {
        lerp_rgba(orange, hot, (h - 0.72) / 0.28)
    }
}
