//! Block elements, box drawing, braille, sextants and powerline arrows are drawn by the GPU as procedural sprites
//! (`sprite_fs` in shaders/harness.slang), never from the font, as Ghostty, kitty and wezterm do: a font's versions
//! overhang or fall short of the cell, so halves miss each other and rows show seams. This file is only the table from
//! a code point to its [`Shape`]; the geometry is Ghostty's (src/font/sprite/draw: box.zig, block.zig, braille.zig,
//! symbols_for_legacy_computing.zig, powerline.zig; github.com/ghostty-org/ghostty, MIT), ported to the shader.

/// What the sprite shader draws in a cell: `kind` and two parameters, as `SpriteInst.shape.xyz`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    pub kind: u32,
    pub a: u32,
    pub b: u32,
}

/// Shape kinds; the shader's `SPRITE_*` constants.
pub mod kind {
    /// A block against one side: `a` the side (0 left, 1 right, 2 top, 3 bottom), `b` its size in eighths.
    pub const BLOCK: u32 = 1;
    /// The whole cell at `a`/255 of the ink (the shades, and the full block at 255).
    pub const SHADE: u32 = 2;
    /// A 2-column grid of `b` rows (2: quadrants, 3: sextants): bit `i` of `a` fills column `i % 2`, row `i / 2`.
    pub const MOSAIC: u32 = 3;
    /// Ghostty's `linesChar`: `a` the arms, 2 bits each (up, right << 2, down << 4, left << 6): 0 none, 1 light,
    /// 2 heavy, 3 double.
    pub const LINES: u32 = 4;
    /// Dashes: `a` how many (2..=4), `b` vertical (bit 0), heavy (bit 1), and the gap (bits 2..: 0 max(4, light),
    /// 1 light, 2 heavy).
    pub const DASH: u32 = 5;
    /// A rounded corner: `a` the corner its arc bends toward, as Ghostty names them (0 tl, 1 tr, 2 bl, 3 br).
    pub const ARC: u32 = 6;
    /// Diagonals: bit 0 of `a` upper right to lower left, bit 1 upper left to lower right.
    pub const DIAGONAL: u32 = 7;
    /// Braille: `a` the dots, the code point's low byte.
    pub const BRAILLE: u32 = 8;
    /// Powerline: `a` 0 the solid triangle, 1 the chevron, 4 the solid half disc, 5 its outline; `b` 1 mirrored.
    pub const POWERLINE: u32 = 9;
}

/// Ghostty's `Lines` of U+2500..=U+254B, packed as [`kind::LINES`] takes them (0: a dash, drawn as [`kind::DASH`]).
const LINES_2500: [u8; 76] = [
    0x44, 0x88, 0x11, 0x22, 0, 0, 0, 0, 0, 0, 0, 0, 0x14, 0x18, 0x24, 0x28, // 2500-250F
    0x50, 0x90, 0x60, 0xA0, 0x05, 0x09, 0x06, 0x0A, 0x41, 0x81, 0x42, 0x82, 0x15, 0x19, 0x16,
    0x25, // 2510-251F
    0x26, 0x1A, 0x29, 0x2A, 0x51, 0x91, 0x52, 0x61, 0x62, 0x92, 0xA1, 0xA2, 0x54, 0x94, 0x58,
    0x98, // 2520-252F
    0x64, 0xA4, 0x68, 0xA8, 0x45, 0x85, 0x49, 0x89, 0x46, 0x86, 0x4A, 0x8A, 0x55, 0x95, 0x59,
    0x99, // 2530-253F
    0x56, 0x65, 0x66, 0x96, 0x5A, 0xA5, 0x69, 0x9A, 0xA9, 0xA6, 0x6A, 0xAA, // 2540-254B
];
/// U+2550..=U+256C, the double lines.
const LINES_2550: [u8; 29] = [
    0xCC, 0x33, 0x1C, 0x34, 0x3C, 0xD0, 0x70, 0xF0, 0x0D, 0x07, 0x0F, 0xC1, 0x43, 0xC3, 0x1D,
    0x37, // 2550-255F
    0x3F, 0xD1, 0x73, 0xF3, 0xDC, 0x74, 0xFC, 0xCD, 0x47, 0xCF, 0xDD, 0x77, 0xFF, // 2560-256C
];
/// U+2574..=U+257F, the half lines.
const LINES_2574: [u8; 12] =
    [0x40, 0x01, 0x04, 0x10, 0x80, 0x02, 0x08, 0x20, 0x48, 0x21, 0x84, 0x12];

/// The sprite `k` is drawn as, if it is one.
#[must_use]
pub fn shape(k: char) -> Option<Shape> {
    let u = k as u32;
    let s = |kind, a, b| Some(Shape { kind, a, b });
    match u {
        0x2500..=0x254B if LINES_2500[(u - 0x2500) as usize] != 0 => {
            s(kind::LINES, u32::from(LINES_2500[(u - 0x2500) as usize]), 0)
        }
        // Dashes: count, then vertical, heavy and the gap (box.zig's dashHorizontal / dashVertical calls).
        0x2504..=0x250B => {
            let i = u - 0x2504;
            s(kind::DASH, if i < 4 { 3 } else { 4 }, (i >> 1) & 1 | (i & 1) << 1)
        }
        0x254C => s(kind::DASH, 2, 1 << 2),
        0x254D => s(kind::DASH, 2, 2 | 2 << 2),
        0x254E => s(kind::DASH, 2, 1 | 2 << 2),
        0x254F => s(kind::DASH, 2, 1 | 2 | 2 << 2),
        0x2550..=0x256C => s(kind::LINES, u32::from(LINES_2550[(u - 0x2550) as usize]), 0),
        // '╭' '╮' '╯' '╰': Ghostty's arc(.br), (.bl), (.tl), (.tr).
        0x256D => s(kind::ARC, 3, 0),
        0x256E => s(kind::ARC, 2, 0),
        0x256F => s(kind::ARC, 0, 0),
        0x2570 => s(kind::ARC, 1, 0),
        0x2571..=0x2573 => s(kind::DIAGONAL, u - 0x2570, 0),
        0x2574..=0x257F => s(kind::LINES, u32::from(LINES_2574[(u - 0x2574) as usize]), 0),
        // Block elements (block.zig).
        0x2580 => s(kind::BLOCK, 2, 4),
        0x2581..=0x2587 => s(kind::BLOCK, 3, u - 0x2580),
        0x2588 => s(kind::SHADE, 255, 0),
        0x2589..=0x258F => s(kind::BLOCK, 0, 0x2590 - u),
        0x2590 => s(kind::BLOCK, 1, 4),
        0x2591 => s(kind::SHADE, 0x40, 0),
        0x2592 => s(kind::SHADE, 0x80, 0),
        0x2593 => s(kind::SHADE, 0xC0, 0),
        0x2594 => s(kind::BLOCK, 2, 1),
        0x2595 => s(kind::BLOCK, 1, 1),
        0x2596..=0x259F => {
            // Quadrants: bits tl, tr, bl, br.
            const Q: [u32; 10] =
                [0b0100, 0b1000, 0b0001, 0b1101, 0b1001, 0b0111, 0b1011, 0b0010, 0b0110, 0b1110];
            s(kind::MOSAIC, Q[(u - 0x2596) as usize], 2)
        }
        0x2800..=0x28FF => s(kind::BRAILLE, u & 0xFF, 0),
        // Sextants: bits tl, tr, ml, mr, bl, br, skipping the two patterns that are the left and right halves.
        0x1FB00..=0x1FB3B => {
            let i = u - 0x1FB00;
            s(kind::MOSAIC, i + i / 0x14 + 1, 3)
        }
        0xE0B0..=0xE0B7 => {
            let i = u - 0xE0B0;
            // E0B0 , E0B1 , E0B2 / E0B3 the same mirrored; E0B4 , E0B5 , E0B6 / E0B7 mirrored.
            s(kind::POWERLINE, (i & 4) | (i & 1), (i >> 1) & 1)
        }
        _ => None,
    }
}

thread_local! {
    static THICK: std::cell::Cell<(u32, u32, u32)> = const { std::cell::Cell::new((0, 0, 1)) };
}

/// A light line's thickness in device pixels at font `size` and `scale` pixels a point: the code font's underline
/// thickness rounded to whole pixels, at least 1 (Ghostty's `box_thickness`). Heavy is twice it.
#[must_use]
pub fn thickness(fonts: &crate::text::Fonts, size: f32, scale: f32) -> u32 {
    let key = (size.to_bits(), scale.to_bits());
    let (ks, kc, t) = THICK.with(std::cell::Cell::get);
    if (ks, kc) == key {
        return t;
    }
    let t = (crate::text::mono_underline(fonts, size) * scale).round().max(1.0) as u32;
    THICK.with(|c| c.set((key.0, key.1, t)));
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sextants_skip_the_halves() {
        // 1FB13 is the 20th pattern; 1FB14 skips 21 (the left half, bits tl ml bl).
        assert_eq!(shape('\u{1FB00}').unwrap().a, 0b000001);
        assert_eq!(shape('\u{1FB13}').unwrap().a, 20);
        assert_eq!(shape('\u{1FB14}').unwrap().a, 22);
        assert_eq!(shape('\u{1FB3B}').unwrap().a, 62);
    }

    #[test]
    fn dashes() {
        assert_eq!(shape('\u{2504}').unwrap(), Shape { kind: kind::DASH, a: 3, b: 0 });
        assert_eq!(shape('\u{2505}').unwrap(), Shape { kind: kind::DASH, a: 3, b: 2 });
        assert_eq!(shape('\u{2506}').unwrap(), Shape { kind: kind::DASH, a: 3, b: 1 });
        assert_eq!(shape('\u{250B}').unwrap(), Shape { kind: kind::DASH, a: 4, b: 3 });
    }
}
