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
    app.persist = false;
    // HARNESS_NATIVE_APPEARANCE=light: the light appearance (the window follows the system's or NSApp's).
    app.dark = std::env::var("HARNESS_NATIVE_APPEARANCE").as_deref() != Ok("light");
    if std::env::var("HARNESS_NATIVE_SIDEBAR").as_deref() == Ok("0") {
        app.fold_sidebar_now();
    }
    // A title-bar strip as tall as AppKit's (Tahoe's, 32 points), so the shot lays the window out as the window does.
    app.set_chrome(crate::app::Chrome { strip: 32.0, title_x: 0.0 });
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
    // HARNESS_NATIVE_SHOT_ZOOM: steps of cmd-= (negative: cmd--) from the default zoom.
    let zoom: i32 = std::env::var("HARNESS_NATIVE_SHOT_ZOOM").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    for _ in 0..zoom.unsigned_abs() {
        app.step_zoom(zoom.signum());
    }
    let secs = std::env::var("HARNESS_NATIVE_SHOT_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(2.5);
    // Pointer input (window points): clicks `x,y;x,y` one every 0.4 s from 1 s in, and a drag `x0,y0,x1,y1` at 0.8 s.
    let nums = |s: &str| s.split(',').filter_map(|n| n.trim().parse::<f32>().ok()).collect::<Vec<_>>();
    let mut clicks: Vec<(f32, f32, f32)> = std::env::var("HARNESS_NATIVE_SHOT_CLICK").unwrap_or_default().split(';').map(nums).filter(|v| v.len() == 2).enumerate().map(|(i, v)| (1.0 + 0.4 * i as f32, v[0], v[1])).collect();
    let mut drag = std::env::var("HARNESS_NATIVE_SHOT_DRAG").ok().map(|s| nums(&s)).filter(|v| v.len() == 4);
    // HARNESS_NATIVE_SHOT_HOVER=x,y: the pointer rests there (hover-only parts show).
    let hover = std::env::var("HARNESS_NATIVE_SHOT_HOVER").ok().map(|s| nums(&s)).filter(|v| v.len() == 2);
    let mut up_next = false;
    let mut at = (0.0f32, 0.0f32);
    let (left, none) = (crate::term::Button::Left, crate::app::KeyMods::default());
    let start = Instant::now();
    let tex = renderer.offscreen((w * backing) as usize, (h * backing) as usize);
    loop {
        let t = start.elapsed().as_secs_f32();
        if up_next {
            app.pointer_up(at.0, at.1, left, none);
            up_next = false;
        } else if let Some(d) = drag.take_if(|_| t >= 0.8) {
            app.pointer_down(d[0], d[1], left, 1, none);
            for k in 1..=8 {
                let f = k as f32 / 8.0;
                at = (d[0] + (d[2] - d[0]) * f, d[1] + (d[3] - d[1]) * f);
                app.pointer_move(at.0, at.1, none);
            }
            up_next = true;
        } else if clicks.first().is_some_and(|c| t >= c.0) {
            let (_, x, y) = clicks.remove(0);
            at = (x, y);
            app.pointer_down(x, y, left, 1, none);
            up_next = true;
        }
        if let Some(hv) = &hover
            && clicks.is_empty()
            && !up_next
        {
            app.pointer_move(hv[0], hv[1], none);
        }
        let r = &renderer;
        let draw = app.frame((w, h), backing, 1.0 / 60.0, &mut |x, y, gw, gh, b| r.upload_atlas(x, y, gw, gh, b));
        if start.elapsed().as_secs_f32() >= secs {
            renderer.render(&draw, Dest::Texture(tex.clone()));
            break;
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    let mut px = renderer.read(&tex);
    // The window is clear where the sidebar's vibrancy shows; offscreen there is none, so stand in a flat ground for it.
    let g = crate::look::tone(app.world.world_ref(), "shot_ground");
    let g = if app.dark { g.dark } else { g.light };
    let ground = [g.r, g.g, g.b];
    for c in px.chunks_exact_mut(4) {
        let under = 1.0 - f32::from(c[3]) / 255.0;
        for k in 0..3 {
            c[k] = (f32::from(c[k]) + ground[k] * 255.0 * under).round().min(255.0) as u8;
        }
        c[3] = 255;
    }
    let file = std::fs::File::create(path).map_err(|e| format!("{path}: {e}"))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), (w * backing) as u32, (h * backing) as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().and_then(|mut wr| wr.write_image_data(&px)).map_err(|e| e.to_string())?;
    eprintln!("harness-native: wrote {path}");
    Ok(())
}
