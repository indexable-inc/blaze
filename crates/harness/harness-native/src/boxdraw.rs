//! Block elements, box drawing, braille, sextants and powerline arrows drawn as shapes on the cell box, never from the
//! font, as kitty, alacritty and wezterm do: a font's versions overhang or fall short of the cell, so halves miss each
//! other and rows show seams. Every edge here is on the device-pixel grid, and the cell is a whole number of pixels
//! (term.rs `Grid`), so neighbouring cells meet exactly.
use crate::draw::Draw;
use crate::pack::LineInst;
use crate::term::{Rgba, rect_panel};

/// Whether `k` is drawn here rather than from the font.
#[must_use]
pub fn drawn(k: char) -> bool {
    matches!(k as u32, 0x2500..=0x259F | 0x2800..=0x28FF | 0x1FB00..=0x1FB3B | 0xE0B0..=0xE0B3)
}

/// Line weights of a box-drawing character, up, right, down, left: 0 none, 1 light, 2 heavy, 3 double. Then how many
/// dashes a straight line is cut into (0: solid).
fn arms(k: u32) -> Option<([u8; 4], u8)> {
    // U+2500..U+254B and U+2574..U+257F, in code point order: "urdl".
    const TABLE: [&str; 76] = [
        "0101", "0202", "1010", "2020", "0101", "0202", "1010", "2020", "0101", "0202", "1010", "2020", // 2500-250B
        "0110", "0210", "0120", "0220", "0011", "0012", "0021", "0022", "1100", "1200", "2100", "2200", // 250C-2517
        "1001", "1002", "2001", "2002", "1110", "1210", "2110", "1120", "2120", "2210", "1220", "2220", // 2518-2523
        "1011", "1012", "2011", "1021", "2021", "2012", "1022", "2022", "0111", "0112", "0211", "0212", // 2524-252F
        "0121", "0122", "0221", "0222", "1101", "1102", "1201", "1202", "2101", "2102", "2201", "2202", // 2530-253B
        "1111", "1112", "1211", "1212", "2111", "1121", "2121", "2112", "2211", "1122", "1221", "2212", // 253C-2547
        "1222", "2122", "2221", "2222", // 2548-254B
    ];
    const HALVES: [&str; 12] = ["0001", "1000", "0100", "0010", "0002", "2000", "0200", "0020", "0201", "1020", "0102", "2010"];
    let k = k as usize;
    let dashes = match k {
        0x2504..=0x2507 => 3,
        0x2508..=0x250B => 4,
        0x254C..=0x254F => 2,
        _ => 0,
    };
    let s = match k {
        0x2500..=0x254B => TABLE[k - 0x2500],
        0x254C | 0x254D => ["0101", "0202"][k - 0x254C],
        0x254E | 0x254F => ["1010", "2020"][k - 0x254E],
        0x2574..=0x257F => HALVES[k - 0x2574],
        // Double lines, U+2550..U+256C.
        0x2550..=0x256C => [
            "0303", "3030", "0310", "0130", "0330", "0013", "0031", "0033", "1300", "3100", "3300", "1003", "3001", "3003", // 2550-255D
            "1310", "3130", "3330", "1013", "3031", "3033", "0313", "0131", "0333", "1303", "3101", "3303", "1313", "3131", // 255E-256B
            "3333",
        ][k - 0x2550],
        _ => return None,
    };
    let b = s.as_bytes();
    Some(([b[0] - b'0', b[1] - b'0', b[2] - b'0', b[3] - b'0'], dashes))
}

/// Draw `k` in the cell `r` (x, y, w, h points) in `ink`; `sc` device pixels a point, `thin` a light line's width.
pub fn draw(d: &mut Draw, k: char, r: [f32; 4], ink: Rgba, sc: f32, thin: f32) {
    let px = |v: f32| (v * sc).round() / sc;
    let [x0, y0, w, h] = r;
    let (x1, y1) = (x0 + w, y0 + h);
    // A rectangle by its edges, each on the pixel grid.
    let mut fill = |d: &mut Draw, a: f32, b: f32, c: f32, e: f32, alpha: f32| {
        let (l, t, rr, bb) = (px(a), px(b), px(c), px(e));
        if rr > l && bb > t {
            let mut col = ink;
            col[3] *= alpha;
            d.panel(rect_panel([l, t, rr - l, bb - t], col));
        }
    };
    let u = k as u32;
    match u {
        // Block elements.
        0x2580 => fill(d, x0, y0, x1, y0 + h / 2.0, 1.0),
        0x2581..=0x2587 => fill(d, x0, y1 - h * (u - 0x2580) as f32 / 8.0, x1, y1, 1.0),
        0x2588 => fill(d, x0, y0, x1, y1, 1.0),
        0x2589..=0x258F => fill(d, x0, y0, x0 + w * (0x2590 - u) as f32 / 8.0, y1, 1.0),
        0x2590 => fill(d, x0 + w / 2.0, y0, x1, y1, 1.0),
        0x2591..=0x2593 => fill(d, x0, y0, x1, y1, (u - 0x2590) as f32 * 0.25),
        0x2594 => fill(d, x0, y0, x1, y0 + h / 8.0, 1.0),
        0x2595 => fill(d, x1 - w / 8.0, y0, x1, y1, 1.0),
        0x2596..=0x259F => {
            // Quadrants: bits upper-left, upper-right, lower-left, lower-right.
            const Q: [u8; 10] = [0b0100, 0b1000, 0b0001, 0b1101, 0b1001, 0b0111, 0b1011, 0b0010, 0b0110, 0b1110];
            let q = Q[(u - 0x2596) as usize];
            let (mx, my) = (x0 + w / 2.0, y0 + h / 2.0);
            for (bit, (a, b, c, e)) in [(0b0001, (x0, y0, mx, my)), (0b0010, (mx, y0, x1, my)), (0b0100, (x0, my, mx, y1)), (0b1000, (mx, my, x1, y1))] {
                if q & bit != 0 {
                    fill(d, a, b, c, e, 1.0);
                }
            }
        }
        // Sextants: a 2 x 3 grid, skipping the patterns that are the left and right halves.
        0x1FB00..=0x1FB3B => {
            let mut s = u - 0x1FB00 + 1;
            if s >= 21 {
                s += 1;
            }
            if s >= 42 {
                s += 1;
            }
            let xs = [x0, x0 + w / 2.0, x1];
            let ys = [y0, y0 + h / 3.0, y0 + 2.0 * h / 3.0, y1];
            for i in 0..6 {
                if s & (1 << i) != 0 {
                    let (c, rr) = (i % 2, i / 2);
                    fill(d, xs[c], ys[rr], xs[c + 1], ys[rr + 1], 1.0);
                }
            }
        }
        // Braille: two columns of four dots.
        0x2800..=0x28FF => {
            let bits = u - 0x2800;
            const AT: [(usize, usize); 8] = [(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2), (0, 3), (1, 3)];
            let dot = px((w / 4.0).min(h / 8.0)).max(1.0 / sc);
            for (i, (c, rr)) in AT.iter().enumerate() {
                if bits & (1 << i) != 0 {
                    let cx = x0 + w * (0.25 + 0.5 * *c as f32);
                    let cy = y0 + h * (0.125 + 0.25 * *rr as f32);
                    let (l, t) = (px(cx - dot / 2.0), px(cy - dot / 2.0));
                    let mut p = rect_panel([l, t, dot, dot], ink);
                    p.shape = [dot / 2.0, 2.0, 0.0, 0.0];
                    d.panel(p);
                }
            }
        }
        // Powerline: solid triangles row by row of device pixels, thin chevrons as strokes.
        0xE0B0 | 0xE0B2 => {
            let rows = (h * sc).round() as i32;
            for i in 0..rows {
                let yc = (i as f32 + 0.5) / rows as f32;
                let reach = w * (1.0 - (2.0 * yc - 1.0).abs());
                let (t, b) = (y0 + i as f32 / sc, y0 + (i + 1) as f32 / sc);
                if u == 0xE0B0 {
                    fill(d, x0, t, x0 + reach, b, 1.0);
                } else {
                    fill(d, x1 - reach, t, x1, b, 1.0);
                }
            }
        }
        0xE0B1 | 0xE0B3 => {
            let pts = if u == 0xE0B1 { [[x0, y0], [x1, y0 + h / 2.0], [x0, y1]] } else { [[x1, y0], [x0, y0 + h / 2.0], [x1, y1]] };
            for l in LineInst::stroke(&pts, thin, (ink, ink), 1.0) {
                d.line(l);
            }
        }
        // Arcs: a quarter circle from the edge's middle into the next edge's.
        0x256D..=0x2570 => {
            let (cx, cy) = (px(x0 + w / 2.0 - thin / 2.0) + thin / 2.0, px(y0 + h / 2.0 - thin / 2.0) + thin / 2.0);
            let rad = (w / 2.0).min(h / 2.0);
            // Toward: the arc's two edges (x side, y side).
            let (sx, sy) = match u {
                0x256D => (1.0, 1.0),
                0x256E => (-1.0, 1.0),
                0x256F => (-1.0, -1.0),
                _ => (1.0, -1.0),
            };
            let ex = if sx > 0.0 { x1 } else { x0 };
            let ey = if sy > 0.0 { y1 } else { y0 };
            // Straight pieces from the arc's ends to the edges, then the arc about (cx + sx rad, cy + sy rad).
            let (ox, oy) = (cx + sx * rad, cy + sy * rad);
            let mut pts = vec![[cx, ey]];
            pts.push([cx, oy]);
            for i in 0..=12 {
                let a = std::f32::consts::FRAC_PI_2 * i as f32 / 12.0;
                pts.push([ox - sx * rad * a.cos(), oy - sy * rad * a.sin()]);
            }
            pts.push([ex, cy]);
            for l in LineInst::stroke(&pts, thin, (ink, ink), 1.0) {
                d.line(l);
            }
        }
        0x2571..=0x2573 => {
            if u != 0x2572 {
                for l in LineInst::stroke(&[[x1, y0], [x0, y1]], thin, (ink, ink), 1.0) {
                    d.line(l);
                }
            }
            if u != 0x2571 {
                for l in LineInst::stroke(&[[x0, y0], [x1, y1]], thin, (ink, ink), 1.0) {
                    d.line(l);
                }
            }
        }
        _ => {
            let Some(([up, right, down, left], dashes)) = arms(u) else { return };
            let heavy = px(thin * 2.0).max(thin + 1.0 / sc);
            let weight = |a: u8| match a {
                1 | 3 => thin,
                2 => heavy,
                _ => 0.0,
            };
            // The lines' tops (left edges), centred on the cell's middle and on the pixel grid.
            let mid = |start: f32, len: f32, t: f32| px(start + len / 2.0 - t / 2.0);
            let (cx, cy) = (x0 + w / 2.0, y0 + h / 2.0);
            // How far past the middle a horizontal arm reaches to cover the vertical one's width, and the other way.
            let vw = weight(up).max(weight(down));
            let hw = weight(left).max(weight(right));
            let double_v = up == 3 || down == 3;
            let double_h = left == 3 || right == 3;
            let vreach = if double_v { 1.5 * thin } else { vw / 2.0 };
            let hreach = if double_h { 1.5 * thin } else { hw / 2.0 };
            let hline = |d: &mut Draw, fill: &mut dyn FnMut(&mut Draw, f32, f32, f32, f32, f32), a: f32, b: f32, arm: u8| {
                let t = weight(arm);
                let offs: &[f32] = if arm == 3 { &[-thin, thin] } else { &[0.0] };
                for &o in offs {
                    let top = mid(y0, h, t) + o;
                    if dashes > 0 {
                        let n = f32::from(dashes);
                        let step = (b - a) / n;
                        for i in 0..dashes {
                            let s = a + step * f32::from(i);
                            fill(d, s, top, s + step * 0.6, top + t, 1.0);
                        }
                    } else {
                        fill(d, a, top, b, top + t, 1.0);
                    }
                }
            };
            let vline = |d: &mut Draw, fill: &mut dyn FnMut(&mut Draw, f32, f32, f32, f32, f32), a: f32, b: f32, arm: u8| {
                let t = weight(arm);
                let offs: &[f32] = if arm == 3 { &[-thin, thin] } else { &[0.0] };
                for &o in offs {
                    let left = mid(x0, w, t) + o;
                    if dashes > 0 {
                        let n = f32::from(dashes);
                        let step = (b - a) / n;
                        for i in 0..dashes {
                            let s = a + step * f32::from(i);
                            fill(d, left, s, left + t, s + step * 0.6, 1.0);
                        }
                    } else {
                        fill(d, left, a, left + t, b, 1.0);
                    }
                }
            };
            // A straight line through the cell is one piece, so dashes run evenly across it.
            if left == right && up == 0 && down == 0 && left != 0 {
                hline(d, &mut fill, x0, x1, left);
                return;
            }
            if up == down && left == 0 && right == 0 && up != 0 {
                vline(d, &mut fill, y0, y1, up);
                return;
            }
            if left != 0 {
                hline(d, &mut fill, x0, cx + vreach, left);
            }
            if right != 0 {
                hline(d, &mut fill, cx - vreach, x1, right);
            }
            if up != 0 {
                vline(d, &mut fill, y0, cy + hreach, up);
            }
            if down != 0 {
                vline(d, &mut fill, cy - hreach, y1, down);
            }
        }
    }
}
