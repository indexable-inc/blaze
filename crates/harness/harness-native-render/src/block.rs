//! A `PixelBlock` (harness-native-ui-text-api, prims.flecs `ui.Block`): a small block of pixel art, a game's item
//! icon, whose front face is a material with an emblem on it. Each face is drawn here, never read from a file: a
//! material (a few shades speckled by a hash of the texel, lit along its top and left edges and shaded along its
//! bottom and right) and an emblem of 8 by 8 texels in the material's own ink.
//!
//! When the node's `face` changes the block turns a quarter about its upright axis: the old face narrows toward the
//! left and darkens as it turns away, the new one widens in from the right and brightens, and the block hops a texel
//! at the middle of the turn. At rest it is one still picture and asks for no frames.
use crate::draw::Draw;
use crate::pack::ImageInst;
use harness_native_ui_text_api::PixelBlock;
use std::collections::HashMap;
use std::rc::Rc;

/// The picture's side in texels, and a face's (a texel of room around it, where the hop goes).
const CANVAS: usize = 16;
const FACE: usize = 14;
/// An emblem's side in texels, and where its corner sits on the face.
const EMBLEM: usize = 8;
const EMBLEM_AT: usize = (FACE - EMBLEM) / 2;

/// A face: its name, its material's shades (sRGB), its emblem's two inks (`#` and `+`), and the emblem's rows.
struct Face {
    name: &'static str,
    shades: [[u8; 3]; 4],
    ink: [u8; 3],
    ink2: [u8; 3],
    emblem: [&'static str; EMBLEM],
}

/// Every face, by what a step does (steps.rs `kind_of`), and `think` for a turn with no step running. A name not here
/// draws `tool`.
const FACES: &[Face] = &[
    // A command: obsidian, a green prompt.
    Face {
        name: "exec",
        shades: [[24, 20, 38], [30, 25, 46], [20, 16, 31], [36, 30, 54]],
        ink: [112, 224, 122],
        ink2: [62, 150, 78],
        emblem: ["........", "#.......", ".#......", "..#.....", ".#......", "#..++++.", "........", "........"],
    },
    // A file read: paper, its lines.
    Face {
        name: "read",
        shades: [[222, 214, 190], [232, 225, 204], [212, 203, 178], [226, 218, 196]],
        ink: [92, 80, 62],
        ink2: [150, 138, 116],
        emblem: ["........", "######..", "........", "++++++++", "........", "#####...", "........", "+++++++."],
    },
    // A file edited: oak planks, a pencil.
    Face {
        name: "edit",
        shades: [[168, 132, 82], [182, 146, 94], [154, 120, 72], [176, 140, 88]],
        ink: [250, 214, 92],
        ink2: [62, 44, 28],
        emblem: [".....##.", "....###+", "...###+.", "..###+..", ".###+...", ".+#+....", "+++.....", "++......"],
    },
    // A file written: birch, a plus.
    Face {
        name: "write",
        shades: [[214, 204, 168], [224, 215, 182], [202, 191, 154], [218, 208, 174]],
        ink: [72, 132, 74],
        ink2: [48, 96, 52],
        emblem: ["........", "...##...", "...##...", ".######.", ".######.", "...##...", "...##...", "........"],
    },
    // A file deleted: netherrack, a cross.
    Face {
        name: "delete",
        shades: [[112, 44, 42], [128, 54, 50], [98, 36, 36], [120, 48, 46]],
        ink: [244, 196, 188],
        ink2: [70, 22, 22],
        emblem: ["........", ".##..##.", ".###.##.", "..####..", "..####..", ".##.###.", ".##..##.", "........"],
    },
    // A search: ice, a lens.
    Face {
        name: "search",
        shades: [[142, 186, 232], [158, 198, 240], [128, 174, 224], [150, 192, 236]],
        ink: [28, 54, 96],
        ink2: [236, 246, 255],
        emblem: [".####...", "#++++#..", "#+..+#..", "#+..+#..", "#++++#..", ".#####..", ".....##.", "......##"],
    },
    // A listing: stone, three bars.
    Face {
        name: "list",
        shades: [[124, 124, 126], [136, 136, 138], [112, 112, 114], [130, 130, 132]],
        ink: [232, 232, 234],
        ink2: [70, 70, 72],
        emblem: ["........", "#.#####.", "........", "#.#####.", "........", "#.#####.", "........", "........"],
    },
    // A build, check or test: a furnace, lit.
    Face {
        name: "build",
        shades: [[96, 96, 98], [108, 108, 110], [84, 84, 86], [102, 102, 104]],
        ink: [255, 158, 44],
        ink2: [255, 226, 110],
        emblem: ["........", "........", "..#..#..", ".##.###.", ".#####+.", ".#++++#.", ".#+++##.", "..####.."],
    },
    // A page fetched: grass over water, the world.
    Face {
        name: "net",
        shades: [[52, 108, 188], [62, 120, 200], [44, 96, 174], [56, 114, 194]],
        ink: [98, 178, 84],
        ink2: [70, 142, 62],
        emblem: ["..####..", ".##+###.", "###..+##", "##+...##", "#....###", "##..+###", ".#..###.", "..####.."],
    },
    // A query: lapis, a drum of rows.
    Face {
        name: "sql",
        shades: [[36, 68, 152], [44, 80, 168], [30, 58, 136], [40, 74, 160]],
        ink: [226, 234, 252],
        ink2: [140, 164, 226],
        emblem: [".######.", "#++++++#", ".######.", "#......#", ".######.", "#......#", ".######.", "........"],
    },
    // An agent started: TNT, a rocket.
    Face {
        name: "spawn",
        shades: [[190, 52, 44], [204, 62, 52], [174, 44, 38], [196, 56, 48]],
        ink: [246, 240, 228],
        ink2: [255, 190, 70],
        emblem: ["....##..", "...####.", "...####.", "..#####.", ".######.", "..###...", ".+.#....", "+.+....."],
    },
    // A skill read: an enchanting table's purple, a book.
    Face {
        name: "skill",
        shades: [[74, 46, 118], [86, 56, 132], [64, 38, 104], [80, 50, 126]],
        ink: [240, 228, 196],
        ink2: [176, 150, 226],
        emblem: ["........", ".###.###", "#+++#+++", "#+++#+++", "#+++#+++", "#+++#+++", ".###.###", "........"],
    },
    // Waiting: sand, an hourglass.
    Face {
        name: "wait",
        shades: [[218, 204, 150], [228, 215, 164], [206, 191, 138], [222, 208, 156]],
        ink: [108, 86, 48],
        ink2: [176, 150, 96],
        emblem: [".######.", ".#++++#.", "..#++#..", "...##...", "...##...", "..#..#..", ".#.++.#.", ".######."],
    },
    // A cell of code: redstone, a play mark.
    Face {
        name: "cell",
        shades: [[92, 22, 22], [106, 28, 28], [80, 16, 16], [98, 24, 24]],
        ink: [255, 84, 72],
        ink2: [255, 170, 150],
        emblem: ["........", ".##.....", ".####...", ".######.", ".######.", ".####...", ".##.....", "........"],
    },
    // An agent's own tool: iron, a wrench.
    Face {
        name: "tool",
        shades: [[170, 172, 178], [184, 186, 192], [156, 158, 164], [176, 178, 184]],
        ink: [52, 56, 66],
        ink2: [108, 112, 122],
        emblem: ["..##..#.", ".#....#.", ".#...##.", ".######.", "..###...", ".###....", "###.....", "##......"],
    },
    // Thinking, no step running: amethyst, a slow spiral.
    Face {
        name: "think",
        shades: [[118, 84, 184], [132, 96, 198], [104, 72, 168], [124, 90, 190]],
        ink: [238, 226, 255],
        ink2: [186, 160, 240],
        emblem: ["........", "..####..", ".#....#.", ".#.##.#.", ".#.+#.#.", ".#..+.#.", "..####..", "........"],
    },
];

fn face(name: &str) -> &'static Face {
    FACES.iter().find(|f| f.name == name).unwrap_or_else(|| FACES.iter().find(|f| f.name == "tool").unwrap_or(&FACES[0]))
}

/// A texel's speckle: a small hash of where it is and which face it is on, so a material has grain and no pattern.
fn grain(x: usize, y: usize, seed: usize) -> usize {
    let mut h = (x as u32).wrapping_mul(0x9E37_79B1) ^ (y as u32).wrapping_mul(0x85EB_CA6B) ^ (seed as u32).wrapping_mul(0xC2B2_AE35);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    (h >> 4) as usize
}

fn lit(c: [u8; 3], k: f32) -> [u8; 3] {
    c.map(|v| (f32::from(v) * k).round().clamp(0.0, 255.0) as u8)
}

/// One texel of `f`'s face: its emblem's ink where the emblem covers it, else its material, lit along the top and left
/// edges and shaded along the bottom and right (a block's bevel).
fn texel(f: &Face, seed: usize, x: usize, y: usize) -> [u8; 3] {
    if (EMBLEM_AT..EMBLEM_AT + EMBLEM).contains(&x) && (EMBLEM_AT..EMBLEM_AT + EMBLEM).contains(&y) {
        match f.emblem[y - EMBLEM_AT].as_bytes()[x - EMBLEM_AT] {
            b'#' => return f.ink,
            b'+' => return f.ink2,
            _ => {}
        }
    }
    let c = f.shades[grain(x, y, seed) % f.shades.len()];
    if y == 0 || x == 0 {
        lit(c, 1.2)
    } else if y == FACE - 1 || x == FACE - 1 {
        lit(c, 0.7)
    } else {
        c
    }
}

/// The block as a `CANVAS` square of straight RGBA, `turn` of the way (0 to 1) from face `from` to face `to`.
fn render(from: &str, to: &str, turn: f32) -> Vec<u8> {
    let (a, b) = (face(from), face(to));
    let (sa, sb) = (FACES.iter().position(|f| f.name == a.name).unwrap_or(0), FACES.iter().position(|f| f.name == b.name).unwrap_or(0));
    // Eased, so the turn starts and lands softly; a quarter turn about the upright axis.
    let p = turn.clamp(0.0, 1.0);
    let p = p * p * (3.0 - 2.0 * p);
    let (s, c) = (p * std::f32::consts::FRAC_PI_2).sin_cos();
    // How many columns the old face still has: the two faces share the block's width as their cosines do.
    let wa = ((FACE as f32) * c / (c + s)).round() as usize;
    let hop = usize::from(p > 0.25 && p < 0.75);
    let mut out = vec![0u8; CANVAS * CANVAS * 4]; // alloc: one small picture a frame of a turn, kept while at rest
    for y in 0..FACE {
        for x in 0..FACE {
            let (f, seed, u, k) = if x < wa {
                (a, sa, x * FACE / wa.max(1), 0.5 + 0.5 * c)
            } else {
                (b, sb, (x - wa) * FACE / (FACE - wa).max(1), 0.5 + 0.5 * s)
            };
            // The edge between the two faces is the block's corner: a shade darker.
            let k = if wa > 0 && wa < FACE && x + 1 == wa { k * 0.75 } else { k };
            let rgb = lit(texel(f, seed, u.min(FACE - 1), y), k);
            let at = ((1 + y - hop) * CANVAS + 1 + x) * 4;
            out[at..at + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
    }
    out
}

/// One block's turn: the face it turns from, the face it shows, and when the turn began (frame clock).
struct Turn {
    from: String,
    to: String,
    began: f32,
}

/// Every `PixelBlock` node's turn, by entity, and each face's picture at rest.
#[derive(Default)]
pub struct Blocks {
    turns: HashMap<u64, Turn>,
    rest: HashMap<&'static str, Rc<[u8]>>,
    /// A block painted this frame is mid-turn.
    turning: bool,
}

impl Blocks {
    /// Paint node `id`'s block `b` into `rect` at frame time `time`: its texels on whole device pixels (`sc` a point),
    /// centred. A face it did not show last time starts a turn to it.
    pub fn paint(&mut self, d: &mut Draw, id: u64, b: &PixelBlock, rect: [f32; 4], alpha: f32, sc: f32, time: f32) {
        let name = face(b.face.as_str()).name;
        let turn = self.turns.entry(id).or_insert_with(|| Turn { from: name.to_owned(), to: name.to_owned(), began: time }); // alloc: one entry a block in view
        if turn.to != name {
            turn.from = std::mem::replace(&mut turn.to, name.to_owned());
            turn.began = time;
        }
        let p = if b.turn > 0.0 { ((time - turn.began) / b.turn).clamp(0.0, 1.0) } else { 1.0 };
        let texels: Rc<[u8]> = if p < 1.0 && turn.from != turn.to {
            self.turning = true;
            render(&turn.from, &turn.to, p).into()
        } else {
            self.rest.entry(name).or_insert_with(|| render(name, name, 1.0).into()).clone()
        };
        let px = (rect[2].min(rect[3]) * sc / CANVAS as f32).floor().max(1.0);
        let side = CANVAS as f32 * px / sc;
        let snap = |v: f32| (v * sc).round() / sc;
        let at = [snap(rect[0] + (rect[2] - side) * 0.5), snap(rect[1] + (rect[3] - side) * 0.5), side, side];
        let size = [CANVAS as f32, CANVAS as f32, alpha, 0.0];
        d.image(ImageInst { rect: at, size, clip: [0.0; 4], clipx: [0.0; 4], light: [0.0; 4], hdr: [0.0; 4] }, texels);
    }

    /// After a frame: whether a block painted in it is still turning (the host draws another).
    pub fn end_frame(&mut self) -> bool {
        std::mem::take(&mut self.turning)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every emblem is a full `EMBLEM` square, a turn starts as the old face and ends as the new one, and mid-turn both
    /// faces show.
    #[test]
    fn faces_are_whole_and_a_turn_goes_from_one_to_the_other() {
        for f in FACES {
            assert!(f.emblem.iter().all(|r| r.len() == EMBLEM), "{}'s emblem is not {EMBLEM} wide", f.name);
        }
        assert_eq!(render("exec", "net", 0.0), render("exec", "exec", 1.0), "a turn starts as the face it leaves");
        assert_eq!(render("exec", "net", 1.0), render("net", "net", 1.0), "a turn ends as the face it shows");
        let mid = render("exec", "net", 0.5);
        assert!(mid != render("exec", "exec", 1.0) && mid != render("net", "net", 1.0), "mid-turn shows both");
        assert_eq!(face("no such kind").name, "tool");
    }
}
