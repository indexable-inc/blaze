//! Offscreen frames to a PNG (`HARNESS_NATIVE_SHOT=<path>`): the same App and Renderer as the window, drawn into a texture
//! and read back, after the daemon has had a moment to send its list and the selected terminal's screen.
use crate::app::App;
use crate::gpu::{Dest, Renderer};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub fn run(path: &str) -> Result<(), String> {
    let (w, h, backing) = (1280.0_f32, 800.0_f32, 2.0_f32);
    let mut renderer = Renderer::new(objc2_metal::MTLCreateSystemDefaultDevice().ok_or("no Metal device")?);
    let mut app = App::new(Arc::new(|| {}))?;
    if std::env::var_os("HARNESS_NATIVE_SHOT_PALETTE").is_some() {
        app.open_palette();
    }
    let secs = std::env::var("HARNESS_NATIVE_SHOT_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(2.5);
    let start = Instant::now();
    let tex = renderer.offscreen((w * backing) as usize, (h * backing) as usize);
    loop {
        let r = &renderer;
        let draw = app.frame((w, h), backing, 1.0 / 60.0, &mut |x, y, gw, gh, b| r.upload_atlas(x, y, gw, gh, b));
        if start.elapsed().as_secs_f32() >= secs {
            renderer.render(&draw, Dest::Texture(tex.clone()));
            break;
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    let px = renderer.read(&tex);
    let file = std::fs::File::create(path).map_err(|e| format!("{path}: {e}"))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), (w * backing) as u32, (h * backing) as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().and_then(|mut wr| wr.write_image_data(&px)).map_err(|e| e.to_string())?;
    eprintln!("harness-native: wrote {path}");
    Ok(())
}
