//! harness-video: a video from a flecs scene (README.md). The scene is the native stack's UI (ui.* prims, layout, motion,
//! draw) with a root that carries [`Video`] and a timeline of `Cue`s as data; this drives it on a fixed frame clock
//! (frame `i` is at `i / fps` seconds, never the wall clock), draws each frame with the app's Metal renderer offscreen and
//! pipes the pixels to ffmpeg.
//!
//!     harness-video SCENE.flecs OUT.mp4            the whole video (H.264 High, yuv420p, BT.709, faststart)
//!     harness-video SCENE.flecs OUT.png AT         one frame, at AT seconds
//!     harness-video SCENE.flecs DIR/ EVERY         a frame every EVERY seconds, DIR/<ms>.png (to look the video over)
//!     harness-video SCENE.flecs OUT.wav            the soundtrack alone
//!
//! The soundtrack is the scene's `BlazeMark`'s (harness-native-blaze's `sound`): synthesised offline from the mark's own
//! timeline, deterministic, and muxed in as AAC. A scene without one is silent.
//!
//! The scene's directory holds `tokens.flecs` (the names the prims read: tones, radii, sizes, springs and the Ink, Muted,
//! Plate... prefabs), loaded before the prims and the scene, and the media its `Picture`s name.
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use harness_native_api::Clock;
use harness_native_flecs::prelude::*;
use harness_native_render::draw::Draw;
use harness_native_render::gpu::{Dest, Renderer};
use harness_native_render::pack::Frame;
use harness_native_render::paint::Painter;
use harness_native_render::text::TypeLook;
use harness_native_ui_draw_api::Paint;
use harness_native_ui_input_api::PointerState;
use harness_native_ui_layout_api::Viewport;
use harness_native_ui_node_api::Laid;

/// A scene's video: the root node it is on is the frame, `width` by `height` points drawn at `scale` device pixels a
/// point, `fps` frames a second for `seconds`.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Video {
    pub width: f32,
    pub height: f32,
    pub fps: f32,
    pub seconds: f32,
    pub scale: f32,
}

/// The world with the scene loaded, its painter, and the Video it found.
struct Stage {
    world: World,
    painter: Painter,
    video: Video,
}

fn script_error(e: EntityView<'_>) -> Option<String> {
    e.try_get::<&flecs::Script>(|s| {
        // SAFETY: `error` is null or a NUL-terminated string owned by the EcsScript component, alive for this call.
        (!s.error.is_null())
            .then(|| unsafe { std::ffi::CStr::from_ptr(s.error) }.to_string_lossy().into_owned())
    })
    .flatten()
}

impl Stage {
    fn load(scene: &Path) -> Result<Self, String> {
        harness_native_ecs::init_os_api();
        harness_native_flecs::set_message_source(harness_native_ecs::last_error);
        // SAFETY: the OS API is set, and this binary links flecs (hyperion-flecs-c).
        let world = unsafe { World::init_host() };
        harness_native_ui_prims::register(&world);
        harness_native_api::register(&world);
        harness_native_ui_timeline_api::register(&world);
        world.component::<Video>().writable();
        world.set(Clock::default());
        world.set(PointerState::default());
        world.set(Paint::default());
        let dir = scene.parent().ok_or_else(|| format!("{}: no directory", scene.display()))?;
        let tokens_path = dir.join("tokens.flecs");
        let tokens = std::fs::read_to_string(&tokens_path).map_err(|e| {
            format!("{}: {e} (a scene's directory holds its tokens.flecs)", tokens_path.display())
        })?;
        let mut scripts: Vec<(String, String)> = vec![("00-tokens".into(), tokens)];
        scripts.extend(
            harness_native_ui_prims::SCENES.iter().map(|(n, s)| ((*n).to_owned(), (*s).to_owned())),
        );
        for (name, src) in &scripts {
            let e = world.script_named(&format!("scene::{name}")).build_from_code(src);
            if let Some(err) = script_error(*e) {
                return Err(format!("scene script {name}: {err}"));
            }
        }
        let path = scene.to_str().ok_or_else(|| format!("{}: not UTF-8", scene.display()))?;
        let e = world.script_named("scene::video").build_from_file(path);
        if let Some(err) = script_error(*e) {
            return Err(format!("{path}: {err}"));
        }
        let mut bad = None;
        world.new_query::<&flecs::Script>().each_entity(|e, _| {
            if bad.is_none() {
                bad = script_error(e).map(|err| format!("scene script {}: {err}", e.name()));
            }
        });
        if let Some(err) = bad {
            return Err(err);
        }
        let w = world.world_ref();
        harness_native_ui_layout::install(w);
        harness_native_ui_input::install(w);
        harness_native_ui_motion::install(w);
        harness_native_ui_draw::install(w);
        let mut videos = Vec::new();
        world.new_query::<&Video>().each_entity(|e, v| videos.push((e.path(), *v)));
        let video = match videos.as_slice() {
            [(_, v)] => *v,
            [] => {
                return Err(format!(
                    "{path}: no entity has a Video (the root node of the frame carries one)"
                ));
            }
            many => {
                return Err(format!(
                    "{path}: {} entities have a Video ({}); a scene is one video",
                    many.len(),
                    many.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>().join(", ")
                ));
            }
        };
        if !(video.width > 0.0
            && video.height > 0.0
            && video.fps > 0.0
            && video.seconds > 0.0
            && video.scale > 0.0)
        {
            return Err(format!(
                "{path}: Video {video:?}: width, height, fps, seconds and scale must all be above 0"
            ));
        }
        let (pw, ph) = (video.width * video.scale, video.height * video.scale);
        if pw.fract() != 0.0 || ph.fract() != 0.0 || pw as u32 % 2 != 0 || ph as u32 % 2 != 0 {
            return Err(format!(
                "{path}: Video {video:?} is {pw}x{ph} pixels; yuv420p needs whole, even sides"
            ));
        }
        let mut painter =
            Painter::new(TypeLook { code_scale: 1.0, plain: true, ..TypeLook::default() });
        painter.media = Some(dir.to_path_buf());
        Ok(Self { world, painter, video })
    }

    /// The soundtrack at `sr` Hz: the one `BlazeMark`'s, or None for a scene without one.
    fn sound(&self, sr: u32) -> Result<Option<Vec<[f32; 2]>>, String> {
        use harness_native_ui_text_api::{BlazeMark, Text};
        use harness_native_ui_timeline_api::GlyphsIn;
        let mut marks = Vec::new();
        self.world.new_query::<&BlazeMark>().each_entity(|e, m| marks.push((e.path(), *m)));
        // Each glyph that arrives (a `GlyphsIn`) rings as it does, in time order.
        let mut glints = Vec::new();
        self.world.new_query::<(&GlyphsIn, &Text)>().each(|(g, t)| {
            let n = t.s.as_str().chars().filter(|c| !c.is_whitespace()).count();
            glints.extend((0..n).map(|i| g.at + g.per * i as f32));
        });
        glints.sort_by(f32::total_cmp);
        match marks.as_slice() {
            [] => Ok(None),
            [(_, m)] => Ok(Some(harness_native_blaze::sound::soundtrack(
                sr,
                self.video.seconds,
                m.start,
                (m.exit > m.start).then_some(m.exit),
                &glints,
                7,
            ))),
            many => Err(format!(
                "{} BlazeMarks ({}): the soundtrack follows one",
                many.len(),
                many.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>().join(", ")
            )),
        }
    }

    fn pixels(&self) -> (u32, u32) {
        (
            (self.video.width * self.video.scale) as u32,
            (self.video.height * self.video.scale) as u32,
        )
    }

    /// Step the world to frame `i` (`dt` since the last step) and flatten it into a Draw.
    fn frame(
        &mut self,
        i: u32,
        dt: f32,
        upload: &mut dyn FnMut(u32, u32, u32, u32, &[u8]),
    ) -> Result<Draw, String> {
        let v = self.video;
        let time = i as f32 / v.fps;
        let w = self.world.world_ref();
        self.world.set(Viewport { w: v.width, h: v.height });
        let next = w.try_get::<&Paint>(|p| p.frame).unwrap_or(0).wrapping_add(1);
        self.world.set(Paint { frame: next });
        self.world.set(Clock { time, moving: false });
        harness_native_ui_timeline::apply(w, time)?;
        self.painter.typeset(w);
        self.world.progress_time(dt);
        // Words whose size changed lay out again in this frame (bounded, as the app's frame is).
        for _ in 0..3 {
            if self.painter.typeset(w) == 0 {
                break;
            }
            self.world.progress_time(0.0);
        }
        harness_native_ui_input::end_frame(w);
        let sc = v.scale;
        let mut d = Draw {
            frame: Frame {
                view: [v.width, v.height],
                scale: sc,
                time,
                dark: 1.0,
                live: sc,
                textfx: [1.0, 0.0],
                tune: [0.82, 1.12, 1.0, 0.0],
                slug: [0.0, 1.0, 1.0, 0.0],
            },
            clear: [0.0, 0.0, 0.0, 1.0],
            ..Draw::default()
        };
        let mut errors = Vec::new();
        for (id, p) in harness_native_ui_draw::painted(w) {
            let e = w.entity_from_id(id);
            let Some(laid) = e.try_cloned::<&Laid>() else { continue };
            let laid =
                if p.w > 0.0 || p.h > 0.0 { Laid { x: p.x, y: p.y, w: p.w, h: p.h } } else { laid };
            if p.alpha <= 0.002 || (p.clipped && (p.clip_w <= 0.0 || p.clip_h <= 0.0)) {
                continue;
            }
            if let Err(err) = self.painter.node(&mut d, e, &p, laid, true, false, sc, upload) {
                errors.push(err);
            }
        }
        if !errors.is_empty() {
            return Err(format!("frame {i} ({time:.3} s):\n{}", errors.join("\n")));
        }
        if self.painter.end_frame() {
            return Err(format!(
                "frame {i} ({time:.3} s): the glyph atlas overflowed; the frame would miss glyphs"
            ));
        }
        Ok(d)
    }
}

/// What the command line asked for.
enum Out {
    Video(PathBuf),
    Still(PathBuf, f32),
    Sheet(PathBuf, f32),
    Audio(PathBuf),
}

fn parse(args: &[String]) -> Result<(PathBuf, Out), String> {
    let usage = "usage: harness-video SCENE.flecs OUT.mp4 | OUT.png AT | DIR/ EVERY | OUT.wav";
    let num = |s: &String| s.parse::<f32>().map_err(|e| format!("{s:?}: {e}; {usage}"));
    match args {
        [scene, out] if out.ends_with(".mp4") => Ok((scene.into(), Out::Video(out.into()))),
        [scene, out] if out.ends_with(".wav") => Ok((scene.into(), Out::Audio(out.into()))),
        [scene, out, at] if out.ends_with(".png") => {
            Ok((scene.into(), Out::Still(out.into(), num(at)?)))
        }
        [scene, out, every] if out.ends_with('/') => {
            Ok((scene.into(), Out::Sheet(out.into(), num(every)?)))
        }
        _ => Err(usage.into()),
    }
}

/// RGBA8 as read back, over black (the video has no alpha).
fn opaque(px: &mut [u8]) {
    for c in px.chunks_exact_mut(4) {
        c[3] = 255;
    }
}

fn write_png(path: &Path, px: &[u8], (w, h): (u32, u32)) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .and_then(|mut wr| wr.write_image_data(px))
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (scene, out) = parse(&args)?;
    let mut stage = Stage::load(&scene)?;
    let v = stage.video;
    const SR: u32 = 48_000;
    if let Out::Audio(path) = &out {
        let sound = stage
            .sound(SR)?
            .ok_or_else(|| format!("{}: no BlazeMark, so no soundtrack", scene.display()))?;
        std::fs::write(path, harness_native_blaze::sound::wav(SR, &sound))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        eprintln!("harness-video: wrote {} ({} s at {SR} Hz)", path.display(), v.seconds);
        return Ok(());
    }
    let (pw, ph) = stage.pixels();
    let mut renderer =
        Renderer::new(objc2_metal::MTLCreateSystemDefaultDevice().ok_or("no Metal device")?);
    let tex = renderer.offscreen(pw as usize, ph as usize);
    let total = (v.seconds * v.fps).round() as u32;
    let dt = 1.0 / v.fps;
    // Which frames to write: every one, one, or one every so often. Every frame before the last wanted one is stepped,
    // so springs and transitions are where they would be in the video.
    let wanted: Vec<u32> = match &out {
        Out::Video(_) => (0..total).collect(),
        Out::Audio(_) => unreachable!("written above"),
        Out::Still(_, at) => vec![((at * v.fps).round() as u32).min(total.saturating_sub(1))],
        Out::Sheet(_, every) => {
            let step = ((every * v.fps).round() as u32).max(1);
            (0..total).step_by(step as usize).collect()
        }
    };
    let last = *wanted.last().ok_or("no frames to draw")?;
    let mut sound_file = None;
    let mut ffmpeg = match &out {
        Out::Video(path) => {
            let size = format!("{pw}x{ph}");
            let fps = format!("{}", v.fps);
            let mut cmd = Command::new("ffmpeg");
            cmd.args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "-s",
                &size,
                "-framerate",
                &fps,
                "-i",
                "-",
            ]);
            if let Some(sound) = stage.sound(SR)? {
                let wav = path.with_extension("soundtrack.wav");
                std::fs::write(&wav, harness_native_blaze::sound::wav(SR, &sound))
                    .map_err(|e| format!("{}: {e}", wav.display()))?;
                cmd.arg("-i").arg(&wav).args([
                    "-map", "0:v", "-map", "1:a", "-c:a", "aac", "-b:a", "256k", "-ar", "48000",
                ]);
                sound_file = Some(wav);
            }
            let child = cmd
                .args([
                    "-vf",
                    "scale=out_color_matrix=bt709:out_range=tv,format=yuv420p",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "slow",
                    "-crf",
                    "14",
                    "-profile:v",
                    "high",
                ])
                .args([
                    "-colorspace",
                    "bt709",
                    "-color_primaries",
                    "bt709",
                    "-color_trc",
                    "bt709",
                    "-color_range",
                    "tv",
                    "-movflags",
                    "+faststart",
                ])
                .arg(path)
                .stdin(Stdio::piped())
                .spawn()
                .map_err(|e| format!("ffmpeg: cannot start: {e}"))?;
            Some(child)
        }
        _ => None,
    };
    if let Out::Sheet(dir, _) = &out {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let mut next = wanted.iter().peekable();
    for i in 0..=last {
        let r = &renderer;
        let draw = stage.frame(i, if i == 0 { 0.0 } else { dt }, &mut |x, y, gw, gh, b| {
            r.upload_atlas(x, y, gw, gh, b)
        })?;
        if next.peek() != Some(&&i) {
            continue;
        }
        next.next();
        renderer.render(&draw, Dest::Texture(tex.clone()));
        let mut px = renderer.read(&tex);
        opaque(&mut px);
        match &out {
            Out::Video(_) => {
                let stdin =
                    ffmpeg.as_mut().and_then(|c| c.stdin.as_mut()).ok_or("ffmpeg: no stdin")?;
                stdin.write_all(&px).map_err(|e| format!("ffmpeg: frame {i}: {e}"))?;
            }
            Out::Still(path, _) => write_png(path, &px, (pw, ph))?,
            Out::Sheet(dir, _) => write_png(
                &dir.join(format!("{:05}.png", (i as f32 / v.fps * 1000.0).round() as u32)),
                &px,
                (pw, ph),
            )?,
            Out::Audio(_) => unreachable!("written above"),
        }
    }
    if let Some(mut child) = ffmpeg {
        drop(child.stdin.take());
        let status = child.wait().map_err(|e| format!("ffmpeg: {e}"))?;
        if !status.success() {
            return Err(format!("ffmpeg: exited with {status}"));
        }
    }
    if let Some(wav) = sound_file {
        std::fs::remove_file(&wav).map_err(|e| format!("{}: {e}", wav.display()))?;
    }
    let what = match &out {
        Out::Video(p) => format!("{} ({total} frames, {pw}x{ph} at {} fps)", p.display(), v.fps),
        Out::Still(p, at) => format!("{} (at {at} s)", p.display()),
        Out::Sheet(d, _) => format!("{} ({} frames)", d.display(), wanted.len()),
        Out::Audio(_) => unreachable!("written above"),
    };
    eprintln!("harness-video: wrote {what}");
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("harness-video: {e}");
        std::process::exit(1);
    }
}
