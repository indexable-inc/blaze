//! Offscreen frames to a PNG (`HARNESS_NATIVE_SHOT=<path>`): the same App and Renderer as the window, drawn into a texture
//! and read back, after the daemon has had a moment to send its list and the selected terminal's screen.
//! `HARNESS_NATIVE_SHOT_BACKING` sets the display scale (default 2) and `HARNESS_NATIVE_SHOT_ZOOM` the cmd-= steps.
//! `HARNESS_NATIVE_SHOT_HEADROOM` draws for a display with that EDR headroom (default 1, SDR: the PNG is what it was);
//! the PNG clips past white, so `HARNESS_NATIVE_SHOT_PFM=<path>` also writes the frame as it is, float RGB (a PFM of the
//! extended sRGB-encoded, premultiplied values) and logs its brightest value.
use crate::app::App;
use crate::gpu::{Dest, Pass, Renderer};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub fn run(path: &str) -> Result<(), String> {
    // HARNESS_NATIVE_SHOT_BACKING: device pixels a point (2, a Retina display, unless set; 1 for a standard one).
    let backing = std::env::var("HARNESS_NATIVE_SHOT_BACKING")
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
        .filter(|b| *b > 0.0)
        .unwrap_or(2.0);
    let (w, h) = (1280.0_f32, 800.0_f32);
    let mut renderer =
        Renderer::new(objc2_metal::MTLCreateSystemDefaultDevice().ok_or("no Metal device")?);
    let mut app = App::new(Arc::new(|| {}))?;
    app.persist = false;
    // The canvas is the screen: a windowed overlay is kept on it, as the window keeps one on its screen.
    app.set_screen([0.0, 0.0, w, h]);
    app.set_gpu_clock(renderer.gpu_clock());
    app.set_media(renderer.media());
    // HARNESS_NATIVE_GALLERY=1: the layout-vocabulary examples instead of the app's view (a 32-point title strip as inset);
    // `prims`: the primitives gallery.
    // `genui`: one generative-UI cell's tree (HARNESS_NATIVE_GENUI=<tree.json>), drawn as a chat card draws it.
    let mut genui = None;
    if let Ok(g) = std::env::var("HARNESS_NATIVE_GALLERY") {
        if g == "genui" {
            genui = Some((crate::gallery::load_genui(&app, 32.0)?, crate::gallery::genui_tree()?));
            crate::gallery::genui_cells()?;
        } else {
            crate::gallery::load(&app, 32.0, g == "prims")?;
        }
    }
    // HARNESS_NATIVE_APPEARANCE=light: the light appearance (the window follows the system's or NSApp's).
    app.dark = std::env::var("HARNESS_NATIVE_APPEARANCE").as_deref() != Ok("light");
    // HARNESS_NATIVE_SHOT_HEADROOM=h: a display with h times SDR white of room (an XDR screen at full brightness: 16).
    if let Some(h) = std::env::var("HARNESS_NATIVE_SHOT_HEADROOM").ok().and_then(|s| s.trim().parse::<f32>().ok()) {
        app.headroom = harness_native_render::hdr::Headroom { current: h, potential: h, reference: 0.0 };
    }
    // HARNESS_NATIVE_SHOT_WORKING: every session row and every run row shows at work, with nothing really at work.
    app.shot_working = std::env::var_os("HARNESS_NATIVE_SHOT_WORKING").is_some();
    if std::env::var("HARNESS_NATIVE_SIDEBAR").as_deref() == Ok("0") {
        app.fold_sidebar_now();
    }
    // A title-bar strip as tall as AppKit's (Tahoe's, 32 points), so the shot lays the window out as the window does.
    app.set_chrome(crate::app::Chrome { strip: 32.0, title_x: 0.0 });
    // `1` opens the palette empty; anything else opens it with that query typed.
    if let Ok(q) = std::env::var("HARNESS_NATIVE_SHOT_PALETTE") {
        app.open_palette();
        if q != "1" {
            app.palette_query(&q);
        }
    }
    // HARNESS_NATIVE_SHOT_OPEN `<url>|<path>`: opened as a press on a link to it would (the browser pane on the right).
    if let Ok(t) = std::env::var("HARNESS_NATIVE_SHOT_OPEN") {
        app.open_target(&t, false);
    }
    // HARNESS_NATIVE_SHOT_FIXTURE <json>: a recorded world (the daemon's list and runs, chats' events) instead of the
    // daemon's (App::shot_fixture); run with a private HARNESS_HOME and no daemon.
    if let Ok(f) = std::env::var("HARNESS_NATIVE_SHOT_FIXTURE") {
        app.shot_fixture(&f)?;
    }
    // A session id, a run (`r12`) or one of its stages (`r12.0`).
    if let Ok(s) = std::env::var("HARNESS_NATIVE_SHOT_SELECT") {
        if s == "archive" {
            app.select_archive_later();
        } else if let Some(run) = s.strip_prefix('r') {
            let (n, k) = run.split_once('.').map_or((run, None), |(n, k)| (n, k.parse().ok()));
            if let Ok(n) = n.parse() {
                app.select_run(n, k);
            }
        } else if let Ok(sid) = s.parse() {
            app.select_session(sid);
            // HARNESS_NATIVE_SHOT_CHAT_MENU `model|effort|all`: that chat's model and effort picker open on that menu.
            if let Ok(page) = std::env::var("HARNESS_NATIVE_SHOT_CHAT_MENU") {
                app.shot_chat_pick(sid, &page);
            }
        }
    }
    // HARNESS_NATIVE_SHOT_PLAN `5h=0.08,week=0.79` (a `!` after a share: refused): every chat's status bar shows that plan
    // as if its agent had reported it (src/app/chat/plan.rs), so a shot shows the plan meter before any agent has.
    if let Ok(spec) = std::env::var("HARNESS_NATIVE_SHOT_PLAN") {
        app.set_shot_plan(&spec)?;
    }
    // HARNESS_NATIVE_SHOT_CHROME `strip,title_x`: a title-bar strip as the window would report it (points), so the title
    // bar and its subtitle show; HARNESS_NATIVE_SHOT_UNFOCUSED: drawn as a window that is not key (the title dimmed).
    if let Some(c) = std::env::var("HARNESS_NATIVE_SHOT_CHROME")
        .ok()
        .map(|s| s.split(',').filter_map(|n| n.trim().parse::<f32>().ok()).collect::<Vec<_>>())
        .filter(|v| v.len() == 2)
    {
        app.set_chrome(crate::app::Chrome { strip: c[0], title_x: c[1] });
    }
    if std::env::var_os("HARNESS_NATIVE_SHOT_UNFOCUSED").is_some() {
        app.focused = false;
    }
    // HARNESS_NATIVE_SHOT_ZOOM: steps of cmd-= (negative: cmd--) from the default zoom.
    let zoom: i32 =
        std::env::var("HARNESS_NATIVE_SHOT_ZOOM").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    for _ in 0..zoom.unsigned_abs() {
        app.step_zoom(zoom.signum());
    }
    let secs =
        std::env::var("HARNESS_NATIVE_SHOT_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(2.5);
    // Pointer input (window points): clicks `x,y;x,y` one every 0.4 s from 1 s in, and a drag `x0,y0,x1,y1` at 0.8 s (or at
    // `x0,y0,x1,y1,t` seconds): pressed, moved in 8 steps, a frame drawn mid-drag, then let go.
    let nums =
        |s: &str| s.split(',').filter_map(|n| n.trim().parse::<f32>().ok()).collect::<Vec<_>>();
    let mut clicks: Vec<(f32, f32, f32)> = std::env::var("HARNESS_NATIVE_SHOT_CLICK")
        .unwrap_or_default()
        .split(';')
        .map(nums)
        .filter(|v| v.len() == 2)
        .enumerate()
        .map(|(i, v)| (1.0 + 0.4 * i as f32, v[0], v[1]))
        .collect();
    let mut drag = std::env::var("HARNESS_NATIVE_SHOT_DRAG")
        .ok()
        .map(|s| nums(&s))
        .filter(|v| v.len() == 4 || v.len() == 5);
    let mut up_next = false;
    // HARNESS_NATIVE_SHOT_HOVER `x,y,t`: the pointer moves to (x, y) at t seconds (a hover mid-spring for a shot just after).
    // Several as `x,y,t;x,y,t`, each logged with the pointer's shape it resolves to a frame later (src/app/pointer.rs).
    let mut hovers: Vec<Vec<f32>> = std::env::var("HARNESS_NATIVE_SHOT_HOVER")
        .unwrap_or_default()
        .split(';')
        .map(nums)
        .filter(|v| v.len() == 3)
        .collect();
    let mut hovered: Option<(f32, f32)> = None;
    // HARNESS_NATIVE_SHOT_ROW_MENU `x,y,t[,ctrl]`: a right press on the sidebar row at (x, y) (window points) at t seconds
    // (with a fourth value of 1, a control-click instead), handed on as the window hands one: that row's menu opens at the
    // point (src/app/row_menu.rs), and what it offers is logged. HARNESS_NATIVE_SHOT_KEY `none+down,t` then moves its
    // highlight, `none+return,t` runs it, `none+escape,t` closes it.
    let mut row_menu =
        std::env::var("HARNESS_NATIVE_SHOT_ROW_MENU").ok().map(|s| nums(&s)).filter(|v| v.len() == 3 || v.len() == 4);
    // HARNESS_NATIVE_SHOT_KEY `k,t`: Command-k at t seconds, into this offscreen app only (`w,2.4`: archive the selected
    // object, which kills its session). With a `+`, the keys as a binding names them: `cmd+shift+t,1.5`, `none+down,2`
    // (`none+` a key with no modifiers: a tree arrow, the palette's Return), dispatched as the window would.
    // A Command key bound to an Edit selector (cmd+c, cmd+x, cmd+v) runs what the window's responder would (see `press`).
    // HARNESS_NATIVE_SHOT_TYPE `text,t`: `text` typed at t seconds, into this offscreen app only (the palette's query
    // when it is open, the Archive's search when it has the keyboard).
    let mut typed = std::env::var("HARNESS_NATIVE_SHOT_TYPE").ok().and_then(|s| {
        s.rsplit_once(',').and_then(|(k, t)| Some((k.to_owned(), t.trim().parse::<f32>().ok()?)))
    });
    // Several as `k,t;k,t` (rapid cmd+] presses).
    let mut keys: Vec<(String, f32)> = std::env::var("HARNESS_NATIVE_SHOT_KEY")
        .unwrap_or_default()
        .split(';')
        .filter_map(|s| {
            s.split_once(',').and_then(|(k, t)| Some((k.to_owned(), t.trim().parse::<f32>().ok()?)))
        })
        .collect();
    // HARNESS_NATIVE_SHOT_COPY `t`: Command-C at t seconds, as the window's copy: does, into this offscreen app only: what it
    // copies is logged, never put on the pasteboard, and what it took pulses (with HARNESS_NATIVE_SHOT_SEQ, mid-pulse).
    let mut copy_at = std::env::var("HARNESS_NATIVE_SHOT_COPY").ok().and_then(|s| s.trim().parse::<f32>().ok());
    // HARNESS_NATIVE_SHOT_SEQ `t0,t1`: every frame from t0 to t1 seconds also written, as `<path>-<ms>.png` (time is then
    // counted in frames of 1/60 s, so a slow readback does not skip animation).
    let seq =
        std::env::var("HARNESS_NATIVE_SHOT_SEQ").ok().map(|s| nums(&s)).filter(|v| v.len() == 2);
    let mut frames = 0u32;
    // HARNESS_NATIVE_SHOT_WHEEL `x,y,dy,t[,dx]`: a trackpad swipe of `dy` (and `dx`) points (negative: toward the end)
    // over (x, y) from t seconds, under natural scrolling: a began, changed events over 8 frames, an ended.
    let mut wheel = std::env::var("HARNESS_NATIVE_SHOT_WHEEL")
        .ok()
        .map(|s| nums(&s))
        .filter(|v| v.len() == 4 || v.len() == 5)
        .map(|v| (v, 0usize));
    // HARNESS_NATIVE_SHOT_DROP `x,y,t[,t_off]`: Finder files carried over (x, y) (window points) from t seconds, as the
    // window's dragging callbacks report them (a DropZone there lights), and carried off at t_off. No drag is made.
    let mut carried = std::env::var("HARNESS_NATIVE_SHOT_DROP").ok().map(|s| nums(&s)).filter(|v| v.len() == 3 || v.len() == 4);
    let mut at = (0.0f32, 0.0f32);
    let (left, none) = (crate::term::Button::Left, crate::app::KeyMods::default());
    // HARNESS_NATIVE_GENUI_NEXT=<tree.json>,<secs>: the cell's tree changes to that one at that time (a value gliding to
    // its next, a float opening).
    let mut next = match std::env::var("HARNESS_NATIVE_GENUI_NEXT") {
        Ok(v) => {
            let (path, secs) = v.rsplit_once(',').ok_or_else(|| format!("HARNESS_NATIVE_GENUI_NEXT={v}: not <tree.json>,<secs>"))?;
            let secs: f32 = secs.trim().parse().map_err(|e| format!("HARNESS_NATIVE_GENUI_NEXT={v}: {secs}: {e}"))?;
            let text = std::fs::read_to_string(path).map_err(|e| format!("HARNESS_NATIVE_GENUI_NEXT {path}: {e}"))?;
            Some((secs, serde_json::from_str::<serde_json::Value>(&text).map_err(|e| format!("HARNESS_NATIVE_GENUI_NEXT {path}: {e}"))?))
        }
        Err(_) => None,
    };
    let start = Instant::now();
    let tex = renderer.offscreen((w * backing) as usize, (h * backing) as usize);
    loop {
        // AVFoundation says its players' status and first frames through the main queue, and a shot runs no run loop.
        harness_native_render::media::Media::pump();
        let t = if seq.is_some() { frames as f32 / 60.0 } else { start.elapsed().as_secs_f32() };
        if next.as_ref().is_some_and(|(secs, _)| t >= *secs)
            && let (Some((_, tree)), Some((_, now))) = (&mut genui, next.take())
        {
            *tree = now;
        }
        if let Some((root, tree)) = &genui {
            app.fill_genui_gallery(*root, tree);
        }
        frames += 1;
        // The last hover's shape, resolved by the frame after it moved.
        if let Some((x, y)) = hovered.take() {
            eprintln!("harness-native: hover {x},{y} pointer {}", app.pointer_line());
        }
        if hovers.first().is_some_and(|h| t >= h[2]) {
            let h = hovers.remove(0);
            app.pointer_move(h[0], h[1], crate::app::KeyMods::default());
            hovered = Some((h[0], h[1]));
        }
        if let Some((v, n)) = wheel.as_mut().filter(|(v, _)| t >= v[3]) {
            if *n == 0 {
                app.pointer_move(v[0], v[1], none);
            }
            let across = v.get(4).copied().unwrap_or(0.0);
            let (phase, dx, dy) = match *n {
                0 => (1, 0.0, 0.0),
                1..=8 => (4, across / 8.0, v[2] / 8.0),
                _ => (8, 0.0, 0.0),
            };
            app.scroll_wheel(
                crate::app::Wheel { dx, dy, precise: true, phase, momentum: 0, inverted: true },
                none,
            );
            *n += 1;
            if *n > 9 {
                wheel = None;
            }
        }
        if let Some(v) = carried.as_mut() {
            if v[2] >= 0.0 && t >= v[2] {
                let lit = app.genui_files_over(Some((v[0], v[1])));
                eprintln!("harness-native: shot files over {},{} at {t:.3}: a drop zone takes them: {lit}", v[0], v[1]);
                v[2] = -1.0;
            }
            if v.len() == 4 && v[2] < 0.0 && t >= v[3] {
                app.genui_files_over(None);
                carried = None;
            }
        }
        if copy_at.take_if(|c| t >= *c).is_some() {
            let got = app.copy();
            eprintln!("harness-native: shot copy at {t:.3}: {:?}", got.as_deref().map(|s| s.chars().take(80).collect::<String>()));
        }
        if let Some((text, _)) = typed.take_if(|k| t >= k.1) {
            app.commit_text(&text);
        }
        if let Some(v) = row_menu.take_if(|v| t >= v[2]) {
            let (b, m) = if v.get(3).is_some_and(|c| *c != 0.0) {
                (left, crate::app::KeyMods { ctrl: true, ..none })
            } else {
                (crate::term::Button::Right, none)
            };
            app.pointer_move(v[0], v[1], m);
            app.pointer_down(v[0], v[1], b, 1, m);
            app.pointer_up(v[0], v[1], b, m);
            eprintln!("harness-native: shot row menu at {},{}: {}", v[0], v[1], app.row_menu_line());
        }
        while keys.first().is_some_and(|k| t >= k.1) {
            let (k, _) = keys.remove(0);
            press(&mut app, &k);
        }
        if up_next {
            eprintln!("harness-native: drag let go at {:.0} window shift {:.1}", at.0, app.window_shift());
            app.pointer_up(at.0, at.1, left, none);
            up_next = false;
        } else if let Some(d) = drag.take_if(|d| t >= d.get(4).copied().unwrap_or(0.8)) {
            app.pointer_down(d[0], d[1], left, 1, none);
            for k in 1..=8 {
                let f = k as f32 / 8.0;
                at = (d[0] + (d[2] - d[0]) * f, d[1] + (d[3] - d[1]) * f);
                app.pointer_move(at.0, at.1, none);
                // The trackpad pulses the drag asked for (window.rs plays them; here they are logged).
                for h in app.take_haptics() {
                    eprintln!("harness-native: haptic {h:?} at {:.0}", at.0);
                }
            }
            up_next = true;
        } else if clicks.first().is_some_and(|c| t >= c.0) {
            let (_, x, y) = clicks.remove(0);
            at = (x, y);
            app.pointer_down(x, y, left, 1, none);
            up_next = true;
        }
        let r = &renderer;
        let mut draw = app.frame((w, h), backing, 1.0 / 60.0, &mut |x, y, gw, gh, b| {
            r.upload_atlas(x, y, gw, gh, b)
        });
        let shoot = seq.as_ref().is_some_and(|v| t >= v[0] && t <= v[1]) || t >= secs;
        if shoot && !draw.media.is_empty() {
            // Settled, then drawn again with no time passing, so what the cell says of its media (a shader's status, a
            // video's time) is what the settled pictures show.
            settle_media(&renderer, &draw);
            draw = app.frame((w, h), backing, 0.0, &mut |x, y, gw, gh, b| r.upload_atlas(x, y, gw, gh, b));
        } else if !shoot && !app.stats_shown() {
            // As the window's every frame does: Media builds and steps between the frames a shot keeps.
            renderer.media().borrow_mut().prepare(&draw.media);
        }
        let floats = floats_in_place(&mut app, &draw);
        if seq.as_ref().is_some_and(|v| t >= v[0] && t <= v[1]) {
            render(&mut renderer, &draw, &floats, &tex);
            let px = renderer.read(&tex);
            write_png(
                &format!("{path}-{:05}.png", (t * 1000.0).round() as u32),
                &app,
                &px,
                (w * backing) as u32,
                (h * backing) as u32,
            )?;
            eprintln!("harness-native: seq t={t:.3} {} shift={:.2}", app.probe(), app.window_shift());
        }
        if t >= secs {
            render(&mut renderer, &draw, &floats, &tex);
            break;
        }
        // F3's overlay shown: every frame goes to the GPU, so its GPU time is a real one.
        if app.stats_shown() {
            render(&mut renderer, &draw, &floats, &tex);
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    let px = renderer.read(&tex);
    write_png(path, &app, &px, (w * backing) as u32, (h * backing) as u32)?;
    eprintln!("harness-native: wrote {path}");
    // As the window does when the app quits: the chats' drafts are kept for the next launch.
    app.flush_drafts();
    if let Ok(pfm) = std::env::var("HARNESS_NATIVE_SHOT_PFM") {
        write_pfm(&pfm, &renderer.read_float(&tex), (w * backing) as usize, (h * backing) as usize)?;
    }
    // The tooltip AppKit would show for the hovered node (it isn't drawn offscreen), when a hover was asked for.
    if std::env::var_os("HARNESS_NATIVE_SHOT_HOVER").is_some() {
        eprintln!("harness-native: tooltip {:?}", app.tooltip());
    }
    // HARNESS_NATIVE_MENU_DUMP: the menu bar the window would have, read back from the NSMenu built from the keys.
    if std::env::var_os("HARNESS_NATIVE_MENU_DUMP").is_some() {
        eprint!("harness-native: menu bar\n{}", crate::window::dump_menu(&app.menu()));
    }
    Ok(())
}

/// The frame's windowed overlays (src/app/floats.rs) as a shot shows them: each draw over the whole canvas (the main
/// frame's view, from its corner), so its picture lands where its window would put it. The system shadow a window casts
/// is not drawn here.
fn floats_in_place(app: &mut App, main: &crate::draw::Draw) -> Vec<crate::draw::Draw> {
    app.take_floats()
        .into_iter()
        .map(|mut f| {
            f.draw.frame.view = main.frame.view;
            f.draw.frame.place = [0.0; 4];
            f.draw
        })
        .collect() // alloc: the frame's open overlays, none or one or two
}

/// The window's draw into `tex`, then each windowed overlay's over it, in one command buffer.
fn render(renderer: &mut Renderer, draw: &crate::draw::Draw, floats: &[crate::draw::Draw], tex: &crate::gpu::Tex) {
    let mut passes = Vec::with_capacity(1 + floats.len()); // alloc: a pass per picture, a shot's frame
    passes.push(Pass { draw, dest: Dest::Texture(tex.clone()), over: false });
    passes.extend(floats.iter().map(|f| Pass { draw: f, dest: Dest::Texture(tex.clone()), over: true }));
    renderer.render_passes(&passes);
}

/// The frame's floats (RGBA, rows top first) as a PFM: `PF`, little-endian (scale -1), RGB, rows bottom first. Logs the
/// brightest value written and the linear light it is (over SDR white), so a check can read the headroom used.
fn write_pfm(path: &str, px: &[f32], w: usize, h: usize) -> Result<(), String> {
    let mut out = format!("PF\n{w} {h}\n-1.0\n").into_bytes();
    out.reserve(w * h * 12);
    for row in (0..h).rev() {
        for p in px[row * w * 4..(row + 1) * w * 4].chunks_exact(4) {
            for c in &p[..3] {
                out.extend_from_slice(&c.to_le_bytes());
            }
        }
    }
    std::fs::write(path, &out).map_err(|e| format!("{path}: {e}"))?;
    let top = px.chunks_exact(4).flat_map(|p| p[..3].iter().copied()).filter(|v| v.is_finite()).fold(0.0f32, f32::max);
    eprintln!(
        "harness-native: wrote {path} (brightest {top:.4}, {:.3}x SDR white)",
        harness_native_render::hdr::srgb_decode_ext(top)
    );
    Ok(())
}

/// A frame drawn once shows the cells' Shaders and Videos as they settle, not as the first frame found them: Media pumps and
/// prepares until each visible one has its picture, for at most [`SETTLE`]. The borrow ends before `render` takes it.
fn settle_media(renderer: &Renderer, draw: &crate::draw::Draw) {
    if !draw.media.is_empty() && !renderer.media().borrow_mut().settle(&draw.media, SETTLE) {
        eprintln!("harness-native: shot: the cells' shaders and videos did not settle in {SETTLE:?}; drawn as they are");
    }
}

/// How long a shot waits for its Shaders and Videos to settle.
const SETTLE: Duration = Duration::from_secs(3);

fn write_png(path: &str, app: &App, px: &[u8], w: u32, h: u32) -> Result<(), String> {
    let mut px = px.to_vec();
    // Where the scene paints nothing the window shows what is under it; offscreen there is nothing, so lay the theme's
    // ground under it.
    let g = crate::look::tone(app.world.world_ref(), "ground");
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
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().and_then(|mut wr| wr.write_image_data(&px)).map_err(|e| e.to_string())
}

/// A `HARNESS_NATIVE_SHOT_KEY` key: a Command key through the window's key equivalents (`App::shortcut`), any other
/// through its keyDown (`App::key`), as AppKit would hand them over.
fn press(app: &mut App, spec: &str) {
    let chord = if spec.contains('+') && spec.len() > 1 {
        crate::keys::Chord::parse(&spec.replace("none+", ""))
    } else if spec.len() > 1 && crate::keys::code_of(&spec.to_lowercase()).is_some() {
        // A named key alone (`f3`): no modifiers, as its binding names it.
        Some(crate::keys::Chord { key: spec.to_lowercase(), ..Default::default() })
    } else {
        Some(crate::keys::Chord { key: spec.to_lowercase(), cmd: true, ..Default::default() })
    };
    let Some(c) = chord else {
        eprintln!("harness-native: shot key {spec:?}: not keys");
        return;
    };
    let code = crate::keys::code_of(&c.key).unwrap_or(0);
    let ch = (c.key.chars().count() == 1).then(|| c.key.chars().next()).flatten();
    let out = if c.cmd {
        app.shortcut(code, ch, c.mods())
    } else {
        let text = ch.map(String::from).unwrap_or_default();
        app.key(&crate::term::KeyIn {
            code,
            text: &text,
            unshifted: ch,
            mods: c.mods(),
            action: crate::term::KeyAction::Press,
            composing: false,
        })
    };
    eprintln!("harness-native: shot key {spec}: {out:?}");
    // A selector goes up the responder chain to the view (src/window.rs): this app plays the view's part, with
    // HARNESS_NATIVE_SHOT_CLIP as the pasteboard (`\n` in it a newline), or HARNESS_NATIVE_SHOT_CLIP_PICTURE (a PNG file)
    // as a picture on it; what a copy or cut would put on it is logged.
    if let crate::app::Outcome::Selector(sel) = out {
        let clip = std::env::var("HARNESS_NATIVE_SHOT_CLIP").unwrap_or_default().replace("\\n", "\n");
        let took = match sel.as_str() {
            "copy:" => app.copy(),
            "cut:" => app.cut(),
            "paste:" => {
                match std::env::var("HARNESS_NATIVE_SHOT_CLIP_PICTURE").map(std::fs::read) {
                    Ok(Ok(png)) => app.paste_pictures(&[png]),
                    Ok(Err(e)) => eprintln!("harness-native: shot paste: HARNESS_NATIVE_SHOT_CLIP_PICTURE: {e}"),
                    Err(_) => app.paste(&clip),
                }
                None
            }
            _ => None,
        };
        eprintln!("harness-native: shot {sel} pasteboard {took:?}");
    }
}
