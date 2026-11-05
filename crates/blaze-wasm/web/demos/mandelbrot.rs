//! The Mandelbrot set, compiled in this page by blaze. Click to zoom in on a point. Edit
//! `palette` or `ITERATIONS`: the view stays where you zoomed to.
#![no_std]

unsafe extern "C" {
    fn fill(red: u32, green: u32, blue: u32);
    fn rect(x: f64, y: f64, width: f64, height: f64);
    fn text(text: *const u8, len: usize, x: f64, y: f64);
}

/// The side of the squares the picture is drawn with, in pixels.
const BLOCK: f64 = 6.0;
const ITERATIONS: u32 = 60;

#[derive(Clone, Copy)]
struct Complex {
    re: f64,
    im: f64,
}

impl core::ops::Add for Complex {
    type Output = Complex;
    fn add(self, other: Complex) -> Complex {
        Complex { re: self.re + other.re, im: self.im + other.im }
    }
}

impl core::ops::Mul for Complex {
    type Output = Complex;
    fn mul(self, other: Complex) -> Complex {
        Complex {
            re: self.re * other.re - self.im * other.im,
            im: self.re * other.im + self.im * other.re,
        }
    }
}

impl Complex {
    fn norm(self) -> f64 {
        self.re * self.re + self.im * self.im
    }
}

/// How many steps `z = z * z + c` takes to leave the circle of radius 2, if it does.
fn escape(c: Complex) -> Option<u32> {
    let mut z = Complex { re: 0.0, im: 0.0 };
    (0..ITERATIONS).find(|_| {
        z = z * z + c;
        z.norm() > 4.0
    })
}

fn palette(steps: Option<u32>) -> (u32, u32, u32) {
    match steps {
        None => (12, 12, 20),
        Some(steps) => {
            let shade = steps * 255 / ITERATIONS;
            (shade, shade * shade / 255, 255 - shade / 2)
        }
    }
}

struct View {
    centre: Complex,
    /// The width of the canvas in the plane.
    span: f64,
    size: (f64, f64),
}

static mut VIEW: View =
    View { centre: Complex { re: -0.6, im: 0.0 }, span: 3.6, size: (1.0, 1.0) };

impl View {
    fn at(&self, x: f64, y: f64) -> Complex {
        let (width, height) = self.size;
        let scale = self.span / width;
        Complex {
            re: self.centre.re + (x - width / 2.0) * scale,
            im: self.centre.im + (y - height / 2.0) * scale,
        }
    }
}

fn label(text: &str, x: f64, y: f64) {
    unsafe {
        fill(230, 230, 230);
        crate::text(text.as_ptr(), text.len(), x, y);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn frame(width: f64, height: f64, _dt: f64) {
    let view = unsafe { &mut *(&raw mut VIEW) };
    view.size = (width, height);
    let mut y = 0.0;
    while y < height {
        let mut x = 0.0;
        while x < width {
            let (red, green, blue) = palette(escape(view.at(x + BLOCK / 2.0, y + BLOCK / 2.0)));
            unsafe {
                fill(red, green, blue);
                rect(x, y, BLOCK, BLOCK);
            }
            x += BLOCK;
        }
        y += BLOCK;
    }
    label("click to zoom in", 16.0, 28.0);
}

#[unsafe(no_mangle)]
pub extern "C" fn click(x: f64, y: f64) {
    let view = unsafe { &mut *(&raw mut VIEW) };
    view.centre = view.at(x, y);
    view.span /= 2.0;
}
