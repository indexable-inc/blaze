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
    // A session id, or a run (`r12`) or one of its stages (`r12.0`).
    if let Ok(s) = std::env::var("HARNESS_NATIVE_SHOT_SELECT") {
        if let Some(run) = s.strip_prefix('r') {
            let (n, k) = run.split_once('.').map_or((run, None), |(n, k)| (n, k.parse().ok()));
            if let Ok(n) = n.parse() {
                app.select_run(n, k);
            }
        } else if let Ok(sid) = s.parse() {
            app.select_session(sid);
        }
    }
    let secs = std::env::var("HARNESS_NATIVE_SHOT_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(2.5);
    // Pointer input (window points): clicks `x,y;x,y` one every 0.4 s from 1 s in, and a drag `x0,y0,x1,y1` at 0.8 s.
    let nums = |s: &str| s.split(',').filter_map(|n| n.trim().parse::<f32>().ok()).collect::<Vec<_>>();
    let mut clicks: Vec<(f32, f32, f32)> = std::env::var("HARNESS_NATIVE_SHOT_CLICK").unwrap_or_default().split(';').map(nums).filter(|v| v.len() == 2).enumerate().map(|(i, v)| (1.0 + 0.4 * i as f32, v[0], v[1])).collect();
    let mut drag = std::env::var("HARNESS_NATIVE_SHOT_DRAG").ok().map(|s| nums(&s)).filter(|v| v.len() == 4);
    let mut up_next = false;
    let start = Instant::now();
    let tex = renderer.offscreen((w * backing) as usize, (h * backing) as usize);
    loop {
        let t = start.elapsed().as_secs_f32();
        if up_next {
            app.pointer_up();
            up_next = false;
        } else if let Some(d) = drag.take_if(|_| t >= 0.8) {
            app.pointer_down(d[0], d[1]);
            for k in 1..=8 {
                let f = k as f32 / 8.0;
                app.pointer_move(d[0] + (d[2] - d[0]) * f, d[1] + (d[3] - d[1]) * f);
            }
            up_next = true;
        } else if clicks.first().is_some_and(|c| t >= c.0) {
            let (_, x, y) = clicks.remove(0);
            app.pointer_down(x, y);
            up_next = true;
        }
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
