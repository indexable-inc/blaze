//! Design tokens matching `ix.dev/blaze` (`site/styles/tokens.css`).
//!
//! Warm neutral charcoal grayscale ladder (`--g-0`..`--g-13`), monochrome-first
//! typography (`--ink-1`..`--ink-4`), and colour reserved strictly for meaning:
//! `--fire` (`#e0461a`), `--ok` (`#4cc27a`), `--danger` (`#ff7168`), and the
//! validated chart/icon series (`--series-1..3`).

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

// =============================================================================
// Exact `ix.dev/blaze` grayscale ladder (`site/styles/tokens.css`)
// =============================================================================

/// `--bg` (`--g-1`): `#101011`
pub const BG_CANVAS: [f32; 4] = rgb(0x10, 0x10, 0x11);
pub const BG_CANVAS_RGB: [f32; 3] = [0x10 as f32 / 255.0, 0x10 as f32 / 255.0, 0x11 as f32 / 255.0];

/// Seamless top bar (`--bg`).
pub const BG_TOPBAR: [f32; 4] = rgb(0x10, 0x10, 0x11);
/// `--bg-2` (`--g-2`): `#141415` — recessed command box & terminal well (like `$ curl ... | sh`).
pub const BG_SURFACE: [f32; 4] = rgb(0x14, 0x14, 0x15);
/// `--bg-3` (`--g-3`): `#19191a` — inline file/folder/code chips (like `target/`, `build.rs`).
pub const BG_CHIP: [f32; 4] = rgb(0x19, 0x19, 0x1a);
/// `--bg-4` (`--g-4`): `#1f1f20` — subtle button & progress track fill.
pub const BG_TRACK: [f32; 4] = rgb(0x1f, 0x1f, 0x20);
/// Floating popup surface `#161617`.
pub const BG_POPUP: [f32; 4] = rgb(0x16, 0x16, 0x17);
/// Selection / hover highlight (`--s-tint`).
pub const BG_SELECTED: [f32; 4] = rgba(0xeb, 0xea, 0xe6, 0.06);
pub const BG_SELECTION: [f32; 4] = rgba(0xeb, 0xea, 0xe6, 0.14);

/// `--line` (`--g-5`): `#28282a`
pub const LINE: [f32; 4] = rgb(0x28, 0x28, 0x2a);
/// Subtle separator between blocks (`#1c1c1e`).
pub const LINE_FAINT: [f32; 4] = rgb(0x1c, 0x1c, 0x1e);
/// `--line-2` (`--g-6`): `#333335`
pub const LINE_STRONG: [f32; 4] = rgb(0x33, 0x33, 0x35);

/// Soft drop shadow for floating popups only.
pub const SHADOW_SOFT: [f32; 4] = rgba(0x00, 0x00, 0x00, 0.55);

// =============================================================================
// Exact `ix.dev/blaze` warm-stone ink ladder
// =============================================================================

/// `--ink-1` (`--g-13`): `#ebeae6` — warm paper-white primary ink.
pub const INK_1: [f32; 4] = rgb(0xeb, 0xea, 0xe6);
/// `--ink-2` (`--g-11`): `#b3b2af` — secondary body & values.
pub const INK_2: [f32; 4] = rgb(0xb3, 0xb2, 0xaf);
/// `--ink-3`: `#878684` — headers, flags, timestamps, keys.
pub const INK_3: [f32; 4] = rgb(0x87, 0x86, 0x84);
/// `--ink-4` (`--g-8`): `#57575a` — `$`, `#`, dim metadata.
pub const INK_4: [f32; 4] = rgb(0x57, 0x57, 0x5a);

// =============================================================================
// Semantic colours only (`site/styles/tokens.css`)
// =============================================================================

/// `--fire`: `#e0461a` — the brand's fire: the one accent for what matters now.
pub const FIRE: [f32; 4] = rgb(0xe0, 0x46, 0x1a);
pub const FIRE_FAINT: [f32; 4] = rgba(0xe0, 0x46, 0x1a, 0.14);
/// `--ok`: `#4cc27a`
pub const OK: [f32; 4] = rgb(0x4c, 0xc2, 0x7a);
/// `--danger`: `#ff7168`
pub const DANGER: [f32; 4] = rgb(0xff, 0x71, 0x68);
pub const DANGER_FAINT: [f32; 4] = rgba(0xff, 0x71, 0x68, 0.06);

/// `--series-1`: `#d95926` (Rust / crate icon)
pub const SERIES_ORANGE: [f32; 4] = rgb(0xd9, 0x59, 0x26);
/// `--series-2`: `#3987e5` (Folder / symlink icon, matching `target/`)
pub const SERIES_BLUE: [f32; 4] = rgb(0x39, 0x87, 0xe5);
/// `--series-3`: `#199e70` (Config / duration accent)
pub const SERIES_AQUA: [f32; 4] = rgb(0x19, 0x9e, 0x70);

// Starship prompt semantic accents (clean pastel-vivid on warm `#101011` charcoal).
/// Starship directory accent (crisp cyan-teal).
pub const PROMPT_DIR: [f32; 4] = rgb(0x6e, 0xd8, 0xce);
/// Starship git branch accent (soft violet-lavender).
pub const PROMPT_BRANCH: [f32; 4] = rgb(0xc7, 0x9b, 0xf2);
/// Starship git status `[?!+⇡]` badge accent (soft coral-rose).
pub const PROMPT_STATUS: [f32; 4] = rgb(0xf2, 0x7d, 0x9a);
/// Starship `❯` ok prompt accent (mint green).
pub const PROMPT_OK: [f32; 4] = rgb(0x7a, 0xd8, 0x96);
/// Starship sparkle / indicator accent (soft sky-periwinkle).
pub const PROMPT_SPARK: [f32; 4] = rgb(0x74, 0xb0, 0xf7);
/// Fish-style inline ghost autosuggestion text.
pub const GHOST_INK: [f32; 4] = rgb(0x56, 0x56, 0x5c);
/// Completion & history matched-character highlight (IntelliJ New UI signature vivid blue).
pub const MATCH_HIGHLIGHT: [f32; 4] = rgb(0x58, 0x9d, 0xf6);
/// IntelliJ New UI dark-blue selected completion row fill.
pub const COMPLETION_SEL_BG: [f32; 4] = rgb(0x27, 0x3b, 0x5c);

// Semantic command-position syntax colors (distinct builtin vs external binary vs abbreviation).
/// Builtin / `blaze-sh` native command (`ls`, `open`, `where`, `sort-by`): crisp sky-blue.
pub const CMD_BUILTIN: [f32; 4] = rgb(0x5c, 0xb8, 0xff);
/// External `$PATH` binary (`fd`, `rg`, `claude`, `git`, `vim`): warm IntelliJ method-gold.
pub const CMD_EXTERNAL: [f32; 4] = rgb(0xe5, 0xc0, 0x7b);
/// Command-position abbreviation (`cl`, `gs`, `ll`, `bc`): vivid IntelliJ lavender.
pub const CMD_ABBREV: [f32; 4] = rgb(0xc6, 0x78, 0xdd);
/// Unresolved/unknown command token: soft coral red.
pub const CMD_UNKNOWN: [f32; 4] = rgb(0xe0, 0x6c, 0x75);

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

/// Restrained `ix.dev/blaze` semantic color for structured [`Value`] cells.
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
