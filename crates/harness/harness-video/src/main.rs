//! harness-video: a video from a flecs scene (README.md). The scene is the native stack's UI (ui.* prims, layout, motion,
//! draw) with a root that carries [`Video`] and a timeline of `Cue`s as data; this drives it on a fixed frame clock
//! (frame `i` is at `i / fps` seconds, never the wall clock), draws each frame with the app's Metal renderer offscreen,
//! converts it to Y'CbCr on the GPU straight into the encoder's pixel buffer (`yuv.rs`), and encodes it with VideoToolbox
//! on the media engine into an mp4 by AVAssetWriter (`encode.rs`), as an export preset says (`presets.json`). No ffmpeg.
//!
//!     harness-video SCENE.flecs OUT.mp4 [PRESET]   the whole video, as PRESET (presets.json; with none, the YouTube HDR
//!                                                  master), then read back and held to the preset (check.rs)
//!     harness-video SCENE.flecs OUT.png AT         one frame, at AT seconds
//!     harness-video SCENE.flecs DIR/ EVERY         a frame every EVERY seconds, DIR/<ms>.png (to look the video over)
//!     harness-video SCENE.flecs OUT.wav            the soundtrack alone, as it is in the video
//!     harness-video SCENE.flecs sounds             every sound of the soundtrack, in time order
//!     harness-video check OUT.mp4 SCENE.flecs PRESET   read a finished mp4 back and hold it to the scene and preset
//!
//! The soundtrack is synthesised offline (harness-native-blaze's `sound`), deterministic, placed binaurally: the
//! `BlazeMark`'s score from the mark's own timeline, plus every `Sound` the scene declares under its frame as data. A
//! root with a [`Mix`] has it brought to that loudness under that peak (`loud.rs`). A scene with neither mark nor sound
//! is silent.
//!
//! The scene is loaded into the app's own world (`harness_native::scene::world`: the app's tokens, the `ui.*` prims with
//! the `cue.*` moves and `voice.*` voices, and the `hn.*` widgets), so a video names the same tokens and is made of the
//! same parts as the app, and declares only what is its own. Its directory holds the media its `Picture`s name, and a
//! `data.json` there becomes the scene's `d_*` constants before it loads (`data.rs`).
mod check;
mod data;
mod encode;
mod loud;
mod preset;
mod yuv;

use std::path::{Path, PathBuf};

use harness_native_api::Clock;
use harness_native_flecs::prelude::*;
use harness_native_render::draw::Draw;
use harness_native_render::gpu::{Dest, Renderer};
use harness_native_render::pack::Frame;
use harness_native_render::paint::Painter;
use harness_native_render::text::TypeLook;
use harness_native_ui_draw_api::Paint;
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

/// The soundtrack's master, on the node that carries the [`Video`]: the mix is brought to `lufs` integrated (ITU-R
/// BS.1770) with no true peak over `peak` dBTP. A video with none keeps the synthesiser's own level.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Mix {
    pub lufs: f32,
    pub peak: f32,
}

/// One sound of the soundtrack as the scene declares it, for the listing.
struct Heard {
    path: String,
    note: harness_native_blaze::sound::Note,
}

/// The world with the scene loaded, its painter, and the Video it found.
struct Stage {
    world: World,
    painter: Painter,
    video: Video,
    /// The entity that carries the Video: the frame, and what the soundtrack's sounds are under.
    root: u64,
    /// The root's Mix, if it has one.
    mix: Option<Mix>,
}

impl Stage {
    fn load(scene: &Path) -> Result<Self, String> {
        // The app's own world (its tokens, the prims and the `hn.*` widgets), so a video is made of what the app is.
        let world = harness_native::scene::world()?;
        world.component::<Video>().writable();
        world.component::<Mix>().writable();
        let dir = scene.parent().ok_or_else(|| format!("{}: no directory", scene.display()))?;
        let path = scene.to_str().ok_or_else(|| format!("{}: not UTF-8", scene.display()))?;
        // The scene's numbers, as constants, before the scene names them.
        let numbers = dir.join("data.json");
        if numbers.exists() {
            let json = std::fs::read_to_string(&numbers).map_err(|e| format!("{}: {e}", numbers.display()))?;
            let script = data::script(&json).map_err(|e| format!("{}: {e}", numbers.display()))?;
            harness_native::scene::load(&world, "video-data", &script)
                .map_err(|e| format!("{}: as constants: {e}", numbers.display()))?;
        }
        harness_native::scene::load_file(&world, "video", path)?;
        let w = world.world_ref();
        harness_native_ui_layout::install(w);
        harness_native_ui_input::install(w);
        harness_native_ui_motion::install(w);
        harness_native_ui_draw::install(w);
        // The inline components' labels, for the typesetter (which reads no world).
        harness_native_render::inline::install(w);
        let mut videos = Vec::new();
        world.new_query::<&Video>().each_entity(|e, v| videos.push((e.path(), (*e.id(), *v))));
        let (root, video) = match videos.as_slice() {
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
                "{path}: Video {video:?} is {pw}x{ph} pixels; 4:2:0 needs whole, even sides"
            ));
        }
        let mut painter =
            Painter::new(TypeLook { code_scale: 1.0, plain: true, ..TypeLook::default() });
        painter.media = Some(dir.to_path_buf());
        let mix = world.world_ref().entity_from_id(root).try_cloned::<&Mix>();
        Ok(Self { world, painter, video, root, mix })
    }

    /// The soundtrack at `sr` Hz: the one `BlazeMark`'s score (if any) and every `Sound` the scene declares, or None for
    /// a scene with neither. With a `Mix` on the root it is mastered, and what it then measures comes with it.
    fn sound(&self, sr: u32) -> Result<Option<(Vec<[f32; 2]>, Option<loud::Loudness>)>, String> {
        let (mark, glints, heard) = self.score()?;
        if mark.is_none() && heard.is_empty() {
            return Ok(None);
        }
        let notes: Vec<_> = heard.iter().map(|h| h.note).collect();
        let glints = if mark.is_some() { glints } else { Vec::new() };
        let mut mix = harness_native_blaze::sound::soundtrack(sr, self.video.seconds, mark, &glints, &notes, 7);
        let measured = match self.mix {
            None => None,
            Some(m) if sr == loud::RATE => Some(loud::master(&mut mix, f64::from(m.lufs), f64::from(m.peak))?),
            Some(_) => return Err(format!("a Mix is mastered at {} Hz, not {sr}", loud::RATE)),
        };
        Ok(Some((mix, measured)))
    }

    /// What the soundtrack is made of: the mark's start and exit, the glyphs that ring, and the scene's sounds in time
    /// order (then by path, so the mix is the same every run).
    #[expect(clippy::type_complexity, reason = "three parts of one score, used together once")]
    fn score(&self) -> Result<(Option<(f32, Option<f32>)>, Vec<f32>, Vec<Heard>), String> {
        use harness_native_blaze::sound::{Note, Voice as V};
        use harness_native_ui_text_api::{BlazeMark, Text};
        use harness_native_ui_timeline_api::{Gain, GlyphsIn, Lasts, Onset, Pan, Pitch, Sound, Voice};
        let mut marks = Vec::new();
        self.world.new_query::<&BlazeMark>().each_entity(|e, m| marks.push((e.path(), *m)));
        // Each glyph that arrives (a `GlyphsIn`) rings as it does, in time order.
        let mut glints = Vec::new();
        self.world.new_query::<(&GlyphsIn, &Text)>().each(|(g, t)| {
            let n = t.s.as_str().chars().filter(|c| !c.is_whitespace()).count();
            glints.extend((0..n).map(|i| g.at + g.per * i as f32));
        });
        glints.sort_by(f32::total_cmp);
        // The scene's own sounds (a `Sound` and its `Onset` under the frame; the rest default as the voice's prefab would leave them),
        // sorted by time then path so the mix is the same every run.
        let mut notes = Vec::new();
        let mut bad = None;
        self.world.new_query::<(&Sound, &Onset)>().each_entity(|e, (s, o)| {
            // Only the video's own: the app's scene has sounds too (its cues, `hn.sound`).
            if !std::iter::successors(e.parent(), |p| p.parent()).any(|p| *p.id() == self.root) {
                return;
            }
            let dur = e.try_get::<&Lasts>(|l| l.dur).unwrap_or(0.0);
            let freq = e.try_get::<&Pitch>(|p| p.hz).unwrap_or(0.0);
            let amp = e.try_get::<&Gain>(|g| g.amp).unwrap_or(0.0);
            let Pan { pos, sweep } = e.try_get::<&Pan>(|p| *p).unwrap_or_default();
            if !(o.at >= 0.0 && dur >= 0.0 && amp >= 0.0 && freq >= 0.0 && (-1.0..=1.0).contains(&pos)) && bad.is_none() {
                bad = Some(format!(
                    "{}: sound at {} for {dur} s, {freq} Hz, gain {amp}, pan {pos}: Onset, Lasts, Pitch and Gain must be \
                     at least 0 and Pan's pos within -1 to 1",
                    e.path(),
                    o.at
                ));
            }
            let voice = match s.voice {
                Voice::Bell => V::Bell,
                Voice::Mallet => V::Mallet,
                Voice::Bass => V::Bass,
                Voice::Click => V::Click,
                Voice::Thud => V::Thud,
                Voice::Whoosh => V::Whoosh,
                Voice::Fire => V::Fire,
                Voice::Pad => V::Pad,
                Voice::Rise => V::Rise,
            };
            notes.push((e.path(), Note { voice, at: o.at, dur, freq, amp, pan: pos, sweep }));
        });
        if let Some(err) = bad {
            return Err(err);
        }
        notes.sort_by(|a, b| a.1.at.total_cmp(&b.1.at).then_with(|| a.0.cmp(&b.0)));
        let heard: Vec<Heard> = notes.into_iter().map(|(path, note)| Heard { path, note }).collect();
        let mark = match marks.as_slice() {
            [] => None,
            [(_, m)] => Some((m.start, (m.exit > m.start).then_some(m.exit))),
            many => {
                return Err(format!(
                    "{} BlazeMarks ({}): the soundtrack follows one",
                    many.len(),
                    many.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>().join(", ")
                ));
            }
        };
        Ok((mark, glints, heard))
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
                place: [0.0; 4],
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
            if let Err(err) = self.painter.node(&mut d, e, &p, laid, p.dark(true), false, sc, upload) {
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
    Sounds,
}

fn parse(args: &[String]) -> Result<(PathBuf, Out, Option<String>), String> {
    let usage = "usage: harness-video SCENE.flecs OUT.mp4 [PRESET] | OUT.png AT | DIR/ EVERY | OUT.wav | sounds\n       \
                 harness-video check OUT.mp4 SCENE.flecs PRESET";
    let num = |s: &String| s.parse::<f32>().map_err(|e| format!("{s:?}: {e}; {usage}"));
    match args {
        [scene, out] if out.ends_with(".mp4") => Ok((scene.into(), Out::Video(out.into()), None)),
        [scene, out, preset] if out.ends_with(".mp4") => Ok((scene.into(), Out::Video(out.into()), Some(preset.clone()))),
        [scene, out] if out.ends_with(".wav") => Ok((scene.into(), Out::Audio(out.into()), None)),
        [scene, out] if out == "sounds" => Ok((scene.into(), Out::Sounds, None)),
        [scene, out, at] if out.ends_with(".png") => Ok((scene.into(), Out::Still(out.into(), num(at)?), None)),
        [scene, out, every] if out.ends_with('/') => Ok((scene.into(), Out::Sheet(out.into(), num(every)?), None)),
        _ => Err(usage.into()),
    }
}

/// The video's pixels and frame rate under `export`: the preset's own, or the scene's where the preset says 0. Err when
/// the preset's frame is not the scene's shape.
fn fit(v: &mut Video, name: &str, export: &preset::Export) -> Result<(), String> {
    if export.fps > 0 {
        v.fps = export.fps as f32;
    }
    if export.width > 0 {
        let scale = export.width as f32 / v.width;
        if (v.height * scale - export.height as f32).abs() > 0.01 {
            return Err(format!(
                "preset {name} is {}x{} pixels and the scene's frame is {}x{} points: not one shape (a scene is laid out \
                 for one aspect; this one needs a preset of its own)",
                export.width, export.height, v.width, v.height
            ));
        }
        v.scale = scale;
    }
    Ok(())
}

/// Read `path` back and hold it to `export` and the video it was made from; the report's words on success.
fn read_back(path: &Path, export: &preset::Export, (pw, ph): (u32, u32), fps: f32, frames: u32) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let report = check::read(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    check::verify(&report, export, (pw, ph), fps.round() as u32, u64::from(frames))
        .map_err(|e| format!("{}: read back, it is not what the preset says:\n{e}\n{}", path.display(), report.words()))?;
    Ok(report.words())
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
    const SR: u32 = 48_000;
    if let [check, file, scene, name] = args.as_slice()
        && check == "check"
    {
        let export = preset::named(name)?;
        let mut v = Stage::load(Path::new(scene))?.video;
        fit(&mut v, name, &export)?;
        let (pw, ph) = ((v.width * v.scale) as u32, (v.height * v.scale) as u32);
        let frames = (v.seconds * v.fps).round() as u32;
        print!("{}", read_back(Path::new(file), &export, (pw, ph), v.fps, frames)?);
        println!("ok: {file} is preset {name}: {}", export.words());
        return Ok(());
    }
    let (scene, out, preset_name) = parse(&args)?;
    let mut stage = Stage::load(&scene)?;
    let name = preset_name.as_deref().unwrap_or(preset::DEFAULT);
    let export = preset::named(name)?;
    if let Out::Video(_) = &out {
        fit(&mut stage.video, name, &export)?;
    }
    let v = stage.video;
    if let Out::Sounds = &out {
        let (mark, glints, heard) = stage.score()?;
        if let Some((start, exit)) = mark {
            println!("{start:8.3}  the BlazeMark's score (its letters land, the fire catches{}), {} glyphs ringing", exit.map_or(String::new(), |e| format!("; it burns away from {e}")), glints.len());
        }
        for h in &heard {
            let n = h.note;
            println!("{:8.3}  {:<7} {:6.2} s {:8.1} Hz  gain {:.3}  pan {:+.2}{}  {}", n.at, format!("{:?}", n.voice), n.dur, n.freq, n.amp, n.pan, if n.sweep == 0.0 { String::new() } else { format!(" sweep {:+.2}", n.sweep) }, h.path);
        }
        return Ok(());
    }
    if let Out::Audio(path) = &out {
        let (sound, measured) = stage
            .sound(SR)?
            .ok_or_else(|| format!("{}: no BlazeMark and no Sound, so no soundtrack", scene.display()))?;
        std::fs::write(path, harness_native_blaze::sound::wav(SR, &sound))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        eprintln!("harness-video: wrote {} ({} s at {SR} Hz, 32-bit float stereo{})", path.display(), v.seconds, loudness(measured));
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
        Out::Audio(_) | Out::Sounds => unreachable!("written above"),
        Out::Still(_, at) => vec![((at * v.fps).round() as u32).min(total.saturating_sub(1))],
        Out::Sheet(_, every) => {
            let step = ((every * v.fps).round() as u32).max(1);
            (0..total).step_by(step as usize).collect()
        }
    };
    let last = *wanted.last().ok_or("no frames to draw")?;
    let mut measured = None;
    let mut encoder = match &out {
        Out::Video(path) => {
            let sound = match (export.audio, stage.sound(SR)?) {
                (preset::Audio::None, _) => None,
                (_, Some((mix, m))) => {
                    measured = m;
                    Some((SR, mix))
                }
                // AAC-LC always writes a track: digital silence for a scene with no sound.
                (preset::Audio::AacLc, None) => Some((SR, vec![[0.0; 2]; (v.seconds * SR as f32) as usize])),
                (preset::Audio::Alac, None) => None,
            };
            Some(encode::Encoder::new(renderer.device(), path, (pw, ph), v.fps, &export, sound)?)
        }
        _ => None,
    };
    if let Out::Sheet(dir, _) = &out {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let hdr = match &out {
        Out::Video(_) => Some(yuv::Yuv::new(renderer.device(), pw, ph, export.colour)?),
        _ => None,
    };
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
        if let Some(enc) = encoder.as_mut() {
            let target = enc.target()?;
            hdr.as_ref().ok_or("yuv: no colour pass")?.convert(&tex, &target.luma, &target.chroma)?;
            enc.encode(target, i)?;
            if i % 60 == 0 {
                eprintln!("harness-video: frame {i}/{total}");
            }
            continue;
        }
        let mut px = renderer.read(&tex);
        opaque(&mut px);
        match &out {
            Out::Video(_) => unreachable!("encoded above"),
            Out::Still(path, _) => write_png(path, &px, (pw, ph))?,
            Out::Sheet(dir, _) => write_png(
                &dir.join(format!("{:05}.png", (i as f32 / v.fps * 1000.0).round() as u32)),
                &px,
                (pw, ph),
            )?,
            Out::Audio(_) | Out::Sounds => unreachable!("written above"),
        }
    }
    if let Some(enc) = encoder {
        enc.finish()?;
    }
    let peak = hdr.as_ref().map_or(0.0, yuv::Yuv::peak_nits);
    let what = match &out {
        Out::Video(p) => {
            // Every render is read back: what was written is what the preset says, or the render fails.
            let report = read_back(p, &export, (pw, ph), v.fps, total)?;
            format!(
                "{} ({total} frames, {pw}x{ph} at {} fps, preset {name}: {}; brightest pixel {peak:.0} nits{})\nread back:\n{report}",
                p.display(),
                v.fps,
                export.words(),
                loudness(measured)
            )
        }
        Out::Still(p, at) => format!("{} (at {at} s)", p.display()),
        Out::Sheet(d, _) => format!("{} ({} frames)", d.display(), wanted.len()),
        Out::Audio(_) | Out::Sounds => unreachable!("written above"),
    };
    eprintln!("harness-video: wrote {what}");
    Ok(())
}

/// A mastered mix's numbers, for a line of output.
fn loudness(measured: Option<loud::Loudness>) -> String {
    measured.map_or(String::new(), |m| {
        format!("; mix {:.2} LUFS integrated, true peak {:.2} dBTP, sample peak {:.2} dBFS", m.lufs, m.true_peak, m.sample_peak)
    })
}

fn main() {
    if let Err(e) = run() {
        eprintln!("harness-video: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped video loads into the app's scene: a token or prim it names that the app renamed fails here, not at
    /// render. `cargo test -p harness-video`.
    #[test]
    fn launch_loads() {
        let scene = Path::new(env!("CARGO_MANIFEST_DIR")).join("scene/launch.flecs");
        let stage = Stage::load(&scene).unwrap_or_else(|e| panic!("{e}"));
        assert!(stage.sound(8_000).unwrap_or_else(|e| panic!("{e}")).is_some(), "the mark has a soundtrack");
        // Every preset fits it or says why not, and with none named it is the YouTube master at the scene's own size.
        let mut v = stage.video;
        let master = preset::named(preset::DEFAULT).unwrap_or_else(|e| panic!("{e}"));
        fit(&mut v, preset::DEFAULT, &master).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(v, stage.video);
        let x = preset::named("x-landscape-1080p60").unwrap_or_else(|e| panic!("{e}"));
        fit(&mut v, "x-landscape-1080p60", &x).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((v.width * v.scale, v.height * v.scale, v.fps), (1920.0, 1080.0, 60.0));
        let square = preset::named("x-square-1080").unwrap_or_else(|e| panic!("{e}"));
        assert!(fit(&mut v, "x-square-1080", &square).is_err_and(|e| e.contains("not one shape")));
    }

    /// A tally's words group the whole part by three and keep the places asked for (tested here, where flecs is linked:
    /// the api crate that owns `Tally` links none, and a `ScriptString` needs its allocator).
    #[test]
    fn tally_words() {
        use harness_native_ui_timeline_api::Tally;
        let tally = |value: f32, decimals: i32| Tally { value, decimals, unit: Default::default() }.words();
        assert_eq!(tally(1_091_174.0, 0), "1,091,174");
        assert_eq!(tally(21_067.0, 0), "21,067");
        assert_eq!(tally(999.0, 0), "999");
        assert_eq!(tally(1000.0, 0), "1,000");
        assert_eq!(tally(15.1, 1), "15.1");
        assert_eq!(tally(0.08, 2), "0.08");
        assert_eq!(tally(5770.4, 0), "5,770");
        assert_eq!(tally(-1234.5, 1), "-1,234.5");
        assert_eq!(tally(-0.001, 1), "0.0");
    }

    /// A video is made of what the app is: an app widget (`hn.*`) on the app's tokens, moved by a shared `cue.*`, with a
    /// shared `voice.*` sounding. The soundtrack is the video's own sound alone, none of the app's (`hn.sound`'s cues).
    #[test]
    fn app_parts_in_a_video() {
        let dir = std::env::temp_dir().join(format!("harness-video-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        let scene = dir.join("parts.flecs");
        let video = |inside: &str| {
            format!(
                "@tree ChildOf\nclip : ui.ZStack {{\n  Video: {{width: 64, height: 36, fps: 10, seconds: 1, scale: 2}}\n  \
                 Fill: $tone_ground\n  badge : hn.Badge {{\n    cue.FadeIn show(at: 0.2, dur: 0.5)\n  }}\n{inside}}}\n"
            )
        };
        let load = |src: String| {
            std::fs::write(&scene, src).unwrap_or_else(|e| panic!("{}: {e}", scene.display()));
            Stage::load(&scene).unwrap_or_else(|e| panic!("{e}"))
        };
        let silent = load(video(""));
        assert!(silent.sound(8_000).unwrap_or_else(|e| panic!("{e}")).is_none(), "the app's own sounds are not the video's");
        let mut rings = load(video("  ding : voice.Bell {\n    Onset: {at: 0.1}\n  }\n"));
        let (sound, _) = rings.sound(8_000).unwrap_or_else(|e| panic!("{e}")).unwrap_or_else(|| panic!("the bell is silent"));
        assert!(sound.iter().any(|s| s[0] != 0.0 || s[1] != 0.0), "the bell is silent");
        // The cue moves the app's widget: faded out before it starts, there after it ends.
        let fade = |stage: &mut Stage, i: u32| {
            stage.frame(i, 0.1, &mut |_, _, _, _, _| {}).unwrap_or_else(|e| panic!("{e}"));
            let badge = stage.world.world_ref().lookup("clip::badge");
            badge.try_get::<&harness_native_ui_motion_api::Pose>(|p| p.fade).unwrap_or_else(|| panic!("clip.badge has no Pose"))
        };
        assert_eq!(fade(&mut rings, 0), 1.0);
        assert_eq!(fade(&mut rings, 9), 0.0);
        std::fs::remove_dir_all(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    }
}
