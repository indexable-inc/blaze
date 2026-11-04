//! Offscreen frames to a PNG (`HARNESS_NATIVE_SHOT=<path>`): the same App and Renderer as the window, drawn into a texture
//! and read back, after the daemon has had a moment to send its list and the selected terminal's screen.
//! `HARNESS_NATIVE_SHOT_BACKING` sets the display scale (default 2) and `HARNESS_NATIVE_SHOT_ZOOM` the cmd-= steps.
use crate::app::App;
use crate::gpu::{Dest, Renderer};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub fn run(path: &str) -> Result<(), String> {
    // HARNESS_NATIVE_SHOT_BACKING: device pixels a point (2, a Retina display, unless set; 1 for a standard one).
    let backing = std::env::var("HARNESS_NATIVE_SHOT_BACKING").ok().and_then(|s| s.parse::<f32>().ok()).filter(|b| *b > 0.0).unwrap_or(2.0);
    let (w, h) = (1280.0_f32, 800.0_f32);
    let mut renderer = Renderer::new(objc2_metal::MTLCreateSystemDefaultDevice().ok_or("no Metal device")?);
    let mut app = App::new(Arc::new(|| {}))?;
    if std::env::var_os("HARNESS_NATIVE_SHOT_PALETTE").is_some() {
        app.open_palette();
    }
    if let Some(sid) = std::env::var("HARNESS_NATIVE_SHOT_SELECT").ok().and_then(|s| s.parse().ok()) {
        app.select_session(sid);
    }
    // HARNESS_NATIVE_SHOT_ZOOM: steps of cmd-= (negative: cmd--) from the default zoom.
    let zoom: i32 = std::env::var("HARNESS_NATIVE_SHOT_ZOOM").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    for _ in 0..zoom.unsigned_abs() {
        app.step_zoom(zoom.signum());
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
