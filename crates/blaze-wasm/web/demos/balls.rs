//! Compiled in this page by blaze, to WebAssembly. Edit anything: the functions that
//! changed are compiled again and swapped in, and the balls stay where they are.
#![no_std]

// What the page gives the program to draw with.
unsafe extern "C" {
    fn fill(red: u32, green: u32, blue: u32);
    fn rect(x: f64, y: f64, width: f64, height: f64);
    fn circle(x: f64, y: f64, radius: f64);
    fn text(text: *const u8, len: usize, x: f64, y: f64);
}

const MAX: usize = 64;

#[derive(Clone, Copy)]
struct Ball {
    x: f64,
    y: f64,
    dx: f64,
    dy: f64,
    hue: u32,
}

struct World {
    balls: [Ball; MAX],
    len: usize,
    frames: u32,
}

static mut WORLD: World = World {
    balls: [Ball { x: 0.0, y: 0.0, dx: 0.0, dy: 0.0, hue: 0 }; MAX],
    len: 0,
    frames: 0,
};

trait Shape {
    fn draw(&self);
}

impl Shape for Ball {
    fn draw(&self) {
        let (red, green, blue) = color(self.hue);
        unsafe {
            fill(red, green, blue);
            circle(self.x, self.y, RADIUS);
        }
    }
}

const RADIUS: f64 = 14.0;
const GRAVITY: f64 = 900.0;
const BOUNCE: f64 = 0.85;

fn color(hue: u32) -> (u32, u32, u32) {
    let step = hue % 60 * 255 / 60;
    match hue / 60 % 6 {
        0 => (255, step, 0),
        1 => (255 - step, 255, 0),
        2 => (0, 255, step),
        3 => (0, 255 - step, 255),
        4 => (step, 0, 255),
        _ => (255, 0, 255 - step),
    }
}

impl Ball {
    fn step(&mut self, width: f64, height: f64, dt: f64) {
        self.dy += GRAVITY * dt;
        self.x += self.dx * dt;
        self.y += self.dy * dt;
        if self.x < RADIUS || self.x > width - RADIUS {
            self.dx = -self.dx;
            self.x = self.x.clamp(RADIUS, width - RADIUS);
        }
        if self.y > height - RADIUS {
            self.dy = -self.dy * BOUNCE;
            self.y = height - RADIUS;
        }
    }
}

impl World {
    fn add(&mut self, x: f64, y: f64) {
        let at = self.len % MAX;
        let hue = self.len as u32 * 47 % 360;
        let dx = if self.len % 2 == 0 { 160.0 } else { -160.0 };
        self.balls[at] = Ball { x, y, dx, dy: -300.0, hue };
        self.len += 1;
    }

    fn balls(&mut self) -> &mut [Ball] {
        let len = self.len.min(MAX);
        &mut self.balls[..len]
    }
}

fn label(text: &str, x: f64, y: f64) {
    unsafe {
        fill(230, 230, 230);
        crate::text(text.as_ptr(), text.len(), x, y);
    }
}

/// The page calls this for every frame, with the size of the canvas and the seconds since
/// the frame before.
#[unsafe(no_mangle)]
pub extern "C" fn frame(width: f64, height: f64, dt: f64) {
    let world = unsafe { &mut *(&raw mut WORLD) };
    world.frames += 1;
    unsafe {
        fill(24, 26, 32);
        rect(0.0, 0.0, width, height);
    }
    for ball in world.balls() {
        ball.step(width, height, dt);
        ball.draw();
    }
    label("click to drop a ball", 16.0, 28.0);
}

/// The page calls this for a click on the canvas.
#[unsafe(no_mangle)]
pub extern "C" fn click(x: f64, y: f64) {
    let world = unsafe { &mut *(&raw mut WORLD) };
    world.add(x, y);
}
