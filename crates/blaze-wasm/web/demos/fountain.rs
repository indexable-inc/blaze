//! A fountain, compiled in this page by blaze. The numbers below are the ones to play with:
//! change one and the water already in the air keeps flying, under the new rule.
#![no_std]

// What the page gives the program to draw with.
unsafe extern "C" {
    fn fill(red: u32, green: u32, blue: u32);
    fn rect(x: f64, y: f64, width: f64, height: f64);
    fn circle(x: f64, y: f64, radius: f64);
    fn text(text: *const u8, len: usize, x: f64, y: f64);
}

/// Drops that leave the spout in a frame.
const RATE: usize = 6;
/// How fast a drop leaves, in pixels a second.
const SPEED: f64 = 620.0;
/// How far to the sides the spout sprays, as a part of `SPEED`.
const SPREAD: f64 = 0.28;
const GRAVITY: f64 = 900.0;
/// Pushes every drop sideways: try `120.0`.
const WIND: f64 = 0.0;
/// Seconds a drop lives.
const LIFE: f64 = 2.4;
/// How much of its speed a drop keeps when it hits the ground.
const SPLASH: f64 = 0.35;
const SIZE: f64 = 3.5;

const BACKGROUND: (u32, u32, u32) = (16, 20, 34);

/// The color of a drop that has lived `age` of its life, from 0 to 1.
fn color(age: f64) -> (u32, u32, u32) {
    let young = (190.0, 235.0, 255.0);
    let old = (30.0, 90.0, 220.0);
    let mix = |from: f64, to: f64, back: u32| {
        let lit = from + (to - from) * age;
        // It fades into the background as it ages.
        (lit + (back as f64 - lit) * age * age) as u32
    };
    (
        mix(young.0, old.0, BACKGROUND.0),
        mix(young.1, old.1, BACKGROUND.1),
        mix(young.2, old.2, BACKGROUND.2),
    )
}

const MAX: usize = 1024;

#[derive(Clone, Copy)]
struct Drop {
    x: f64,
    y: f64,
    dx: f64,
    dy: f64,
    age: f64,
}

impl Drop {
    const NONE: Drop = Drop { x: 0.0, y: 0.0, dx: 0.0, dy: 0.0, age: LIFE };

    fn alive(&self) -> bool {
        self.age < LIFE
    }

    fn step(&mut self, ground: f64, dt: f64) {
        self.age += dt;
        self.dx += WIND * dt;
        self.dy += GRAVITY * dt;
        self.x += self.dx * dt;
        self.y += self.dy * dt;
        if self.y > ground && self.dy > 0.0 {
            self.y = ground;
            self.dy = -self.dy * SPLASH;
            self.dx *= 0.7;
        }
    }

    fn draw(&self) {
        let age = (self.age / LIFE).clamp(0.0, 1.0);
        let (red, green, blue) = color(age);
        unsafe {
            fill(red, green, blue);
            circle(self.x, self.y, SIZE * (1.0 - 0.6 * age));
        }
    }
}

struct Fountain {
    drops: [Drop; MAX],
    next: usize,
    /// Where a click put the spout. It is in the middle until one does.
    spout: Option<f64>,
    seed: u32,
}

static mut FOUNTAIN: Fountain =
    Fountain { drops: [Drop::NONE; MAX], next: 0, spout: None, seed: 0x2545_f491 };

impl Fountain {
    /// A number from -1 to 1, a different one each time.
    fn random(&mut self) -> f64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 17;
        self.seed ^= self.seed << 5;
        self.seed as f64 / u32::MAX as f64 * 2.0 - 1.0
    }

    fn spray(&mut self, x: f64, y: f64) {
        let dx = self.random() * SPEED * SPREAD;
        let dy = -SPEED * (1.0 + 0.12 * self.random());
        self.drops[self.next] = Drop { x, y, dx, dy, age: 0.0 };
        self.next = (self.next + 1) % MAX;
    }
}

fn label(text: &str, x: f64, y: f64) {
    unsafe {
        fill(200, 215, 240);
        crate::text(text.as_ptr(), text.len(), x, y);
    }
}

/// The page calls this for every frame, with the size of the canvas and the seconds since
/// the frame before.
#[unsafe(no_mangle)]
pub extern "C" fn frame(width: f64, height: f64, dt: f64) {
    let fountain = unsafe { &mut *(&raw mut FOUNTAIN) };
    let ground = height - 24.0;
    let spout = fountain.spout.unwrap_or(width / 2.0).min(width);
    unsafe {
        fill(BACKGROUND.0, BACKGROUND.1, BACKGROUND.2);
        rect(0.0, 0.0, width, height);
    }
    for _ in 0..RATE {
        fountain.spray(spout, ground - 18.0);
    }
    for drop in fountain.drops.iter_mut().filter(|it| it.alive()) {
        drop.step(ground, dt);
        drop.draw();
    }
    unsafe {
        fill(70, 80, 105);
        rect(0.0, ground, width, height - ground);
        rect(spout - 7.0, ground - 18.0, 14.0, 18.0);
    }
    label("click to move the spout", 16.0, 28.0);
}

/// The page calls this for a click on the canvas.
#[unsafe(no_mangle)]
pub extern "C" fn click(x: f64, _y: f64) {
    let fountain = unsafe { &mut *(&raw mut FOUNTAIN) };
    fountain.spout = Some(x);
}
