//! Shelf packer for the glyph atlas (one padded rectangle per glyph).

#[derive(Debug)]
pub struct Packer {
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    shelf_h: u32,
}

/// Padding around every glyph so linear sampling never bleeds a neighbour in.
pub const PAD: u32 = 1;

impl Packer {
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height, x: 0, y: 0, shelf_h: 0 }
    }

    /// How much of the atlas the shelves have taken (0 to 1): the full shelves and the open one up to its pen.
    #[must_use]
    pub fn used(&self) -> f32 {
        let area = f64::from(self.width) * f64::from(self.height);
        if area <= 0.0 {
            return 0.0;
        }
        ((f64::from(self.y) * f64::from(self.width) + f64::from(self.x) * f64::from(self.shelf_h))
            / area) as f32
    }

    /// Top-left of a free `w` by `h` rectangle, or None when the atlas is full.
    pub fn alloc(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        let (pw, ph) = (w + 2 * PAD, h + 2 * PAD);
        if pw > self.width || ph > self.height {
            return None;
        }
        if self.x + pw > self.width {
            self.y += self.shelf_h;
            self.x = 0;
            self.shelf_h = 0;
        }
        if self.y + ph > self.height {
            return None;
        }
        let at = (self.x + PAD, self.y + PAD);
        self.x += pw;
        self.shelf_h = self.shelf_h.max(ph);
        Some(at)
    }
}
