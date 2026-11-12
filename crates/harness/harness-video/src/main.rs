//! harness-video: a video from a flecs scene (README.md). The scene is the native stack's UI (ui.* prims, layout, motion,
//! draw) with a root that carries [`Video`] and a timeline of `Cue`s as data; this drives it on a fixed frame clock
//! (frame `i` is at `i / fps` seconds, never the wall clock), draws each frame with the app's Metal renderer offscreen,
//! converts it to Y'CbCr on the GPU straight into the encoder's pixel buffer (`yuv.rs`), and encodes it with VideoToolbox
//! on the media engine into an mp4 by AVAssetWriter (`encode.rs`), as an export preset says (`presets.json`). No ffmpeg.
//!
//!     harness-video SCENE.flecs OUT.mp4 [PRESET]   the whole video, as PRESET (presets.json; with none, the YouTube HDR
//!                                                  master), then read back and held to the preset (check.rs)
//!     harness-video SCENE.flecs OUT.png AT [PRESET] one frame, at AT seconds (PRESET: its shape, for a scene that reframes)
//!     harness-video SCENE.flecs OUT.png contact [EVERY] [PRESET]  a contact sheet: a small frame each EVERY seconds (1)
//!     harness-video SCENE.flecs OUT.png sound      the mix as a picture: its waveform over its spectrogram
//!     harness-video SCENE.flecs OUT.png burst AT [PRESET]  ten consecutive frames from AT, in one image
//!     harness-video SCENE.flecs DIR/ EVERY         a frame every EVERY seconds, DIR/<ms>.png (to look the video over)
//!     harness-video SCENE.flecs OUT.wav            the soundtrack alone, as it is in the video
//!     harness-video SCENE.flecs sounds             every sound of the soundtrack, in time order
//!     harness-video check OUT.mp4 SCENE.flecs PRESET   read a finished mp4 back and hold it to the scene and preset
//!     harness-video SCENE.flecs cuts               every hard cut and where it is in the music; fails on one off the grid
//!     harness-video analyse TRACK.wav beats.json   a track's beat grid (music.rs), and beats.png: the track with the grid on it
//!     harness-video score DIR                      our own score (music.rs): its mix, stems, page, piano roll, loudness curve
//!
//! Music: a `beats.json` beside the scene becomes the `m_*` constants a scene places its cuts by; a root with a `Music`
//! has `music.wav` beside the scene as its soundtrack, the scene's own sounds mixed under it.
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
mod backdrop;
mod check;
mod data;
mod encode;
mod loud;
mod music;
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
    /// The scene is laid out to be reframed (everything centre-weighted, sizes as shares of the frame): a preset of
    /// another shape (upright, square) redraws it at that shape, in the same points a pixel. Without it such a preset
    /// is refused.
    pub reframes: bool,
}

/// The soundtrack's master, on the node that carries the [`Video`]: the mix is brought to `lufs` integrated (ITU-R
/// BS.1770) with no true peak over `peak` dBTP. A video with none keeps the synthesiser's own level.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Mix {
    pub lufs: f32,
    pub peak: f32,
}

/// The film is cut to a track: `music.wav` beside the scene is its soundtrack, at `gain`, with the scene's own sounds
/// (and a mark's score) mixed in at `under` of their level, so the track stays the hero. On the node that carries the
/// [`Video`]. The file is the owner's to supply (or the temp score, `harness-video score`); a root with a `Music` and no
/// file is an error, never a silent film.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Music {
    pub gain: f32,
    pub under: f32,
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
    /// The root has a Backdrop: the UI is drawn over a clear ground and laid over the picture.
    pictured: bool,
    /// The scene's beat grid (`beats.json` beside it), if it has one.
    grid: Option<music::Grid>,
    /// The root's Music and the track's path, if it is cut to one.
    track: Option<(Music, PathBuf)>,
}

impl Stage {
    fn load(scene: &Path) -> Result<Self, String> {
        // The app's own world (its tokens, the prims and the `hn.*` widgets), so a video is made of what the app is.
        let world = harness_native::scene::world()?;
        world.component::<Video>().writable();
        world.component::<Mix>().writable();
        world.component::<backdrop::Backdrop>().writable();
        world.component::<Music>().writable();
        let dir = scene.parent().ok_or_else(|| format!("{}: no directory", scene.display()))?;
        let path = scene.to_str().ok_or_else(|| format!("{}: not UTF-8", scene.display()))?;
        // The track's grid, as constants, before the scene places its cuts by them.
        let beats = dir.join("beats.json");
        let grid = if beats.exists() {
            let json = std::fs::read_to_string(&beats).map_err(|e| format!("{}: {e}", beats.display()))?;
            let grid: music::Grid = serde_json::from_str(&json).map_err(|e| format!("{}: {e}", beats.display()))?;
            harness_native::scene::load(&world, "video-beats", &grid.constants())
                .map_err(|e| format!("{}: as constants: {e}", beats.display()))?;
            Some(grid)
        } else {
            None
        };
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
        let pictured = world.world_ref().entity_from_id(root).has(backdrop::Backdrop::id());
        let track = world.world_ref().entity_from_id(root).try_cloned::<&Music>().map(|m| (m, dir.join("music.wav")));
        Ok(Self { world, painter, video, root, mix, pictured, grid, track })
    }

    /// Every hard cut (a cue on `Backdrop.shot`: the picture changes whole) with where it is in the music, a line each.
    /// Err, with the list, when one is more than a frame off a beat or a half beat of the grid.
    fn cuts(&self) -> Result<String, String> {
        use harness_native_ui_timeline_api::Cue;
        let grid = self.grid.as_ref().ok_or("the scene has no beats.json beside it: no grid to hold its cuts to")?;
        let mut cuts = Vec::new();
        self.world.new_query::<&Cue>().each_entity(|e, c| {
            if c.member.as_str() == "Backdrop.shot" {
                cuts.push((c.at, e.name().to_string()));
            }
        });
        cuts.sort_by(|a, b| a.0.total_cmp(&b.0));
        let frame = 1.0 / f64::from(self.video.fps);
        let mut out = format!("{} cuts against {} ({:.2} beats a minute):\n", cuts.len(), grid.source, grid.bpm);
        let mut off_grid = 0;
        for (at, name) in &cuts {
            let (said, off) = grid.position(f64::from(*at));
            let ok = off.abs() <= frame + 1e-6;
            off_grid += usize::from(!ok);
            out.push_str(&format!("{at:8.3} s  frame {:>4}  {said:<28} {:+.1} ms  {name}{}\n", (at * self.video.fps).round(), off * 1000.0, if ok { "" } else { "   OFF THE GRID" }));
        }
        if off_grid > 0 { Err(format!("{out}{off_grid} cuts are more than one frame off the grid")) } else { Ok(out) }
    }

    /// The root's Backdrop as the cues left it this frame.
    fn backdrop(&self) -> Option<backdrop::Backdrop> {
        self.world.world_ref().entity_from_id(self.root).try_cloned::<&backdrop::Backdrop>()
    }

    /// Take `export`'s shape and rate (see [`fit`]), and make the root that many points.
    fn fit(&mut self, name: &str, export: &preset::Export) -> Result<(), String> {
        fit(&mut self.video, name, export)?;
        let v = self.video;
        let size = harness_native_ui_layout_api::Size {
            w: harness_native_ui_layout_api::Sizing::fixed(v.width),
            h: harness_native_ui_layout_api::Sizing::fixed(v.height),
        };
        self.world.world_ref().entity_from_id(self.root).set(size);
        Ok(())
    }

    /// The soundtrack at `sr` Hz: the one `BlazeMark`'s score (if any) and every `Sound` the scene declares, or None for
    /// a scene with neither. With a `Mix` on the root it is mastered, and what it then measures comes with it.
    fn sound(&self, sr: u32) -> Result<Option<(Vec<[f32; 2]>, Option<loud::Loudness>)>, String> {
        let (mark, glints, heard) = self.score()?;
        if mark.is_none() && heard.is_empty() && self.track.is_none() {
            return Ok(None);
        }
        let notes: Vec<_> = heard.iter().map(|h| h.note).collect();
        let glints = if mark.is_some() { glints } else { Vec::new() };
        let mut mix = if mark.is_none() && heard.is_empty() {
            vec![[0.0; 2]; (self.video.seconds * sr as f32) as usize]
        } else {
            harness_native_blaze::sound::soundtrack(sr, self.video.seconds, mark, &glints, &notes, 7)
        };
        if let Some((m, path)) = &self.track {
            if sr != loud::RATE {
                return Err(format!("a Music's track is mixed at {} Hz, not {sr}", loud::RATE));
            }
            let bytes = std::fs::read(path).map_err(|e| {
                format!("{}: {e}: the scene's root has a Music, so its track must be there (the owner's file, or `harness-video score` for the temp score)", path.display())
            })?;
            let track = music::read_wav(&bytes, &path.display().to_string())?;
            for (i, s) in mix.iter_mut().enumerate() {
                let t = track.get(i).copied().unwrap_or([0.0; 2]);
                *s = [t[0] * m.gain + s[0] * m.under, t[1] * m.gain + s[1] * m.under];
            }
        }
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
            clear: [0.0, 0.0, 0.0, if self.pictured { 0.0 } else { 1.0 }],
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
    /// A small frame every so many seconds, in one image.
    Contact(PathBuf, f32),
    /// The mix as a picture.
    Heard(PathBuf),
    /// Ten consecutive frames from a time, in one image.
    Burst(PathBuf, f32),
    Sheet(PathBuf, f32),
    Audio(PathBuf),
    Sounds,
    /// Every hard cut and where it is in the music.
    Cuts,
}

fn parse(args: &[String]) -> Result<(PathBuf, Out, Option<String>), String> {
    let usage = "usage: harness-video SCENE.flecs OUT.mp4 [PRESET] | OUT.png AT [PRESET] | OUT.png contact [PRESET] | \
                 OUT.png burst AT [PRESET] | DIR/ EVERY | OUT.wav | sounds\n       \
                 harness-video check OUT.mp4 SCENE.flecs PRESET";
    let num = |s: &String| s.parse::<f32>().map_err(|e| format!("{s:?}: {e}; {usage}"));
    match args {
        [scene, out] if out.ends_with(".mp4") => Ok((scene.into(), Out::Video(out.into()), None)),
        [scene, out, preset] if out.ends_with(".mp4") => Ok((scene.into(), Out::Video(out.into()), Some(preset.clone()))),
        [scene, out] if out.ends_with(".wav") => Ok((scene.into(), Out::Audio(out.into()), None)),
        [scene, out] if out == "sounds" => Ok((scene.into(), Out::Sounds, None)),
        [scene, out] if out == "cuts" => Ok((scene.into(), Out::Cuts, None)),
        [scene, out, how, rest @ ..] if out.ends_with(".png") => {
            let (what, rest) = match (how.as_str(), rest) {
                ("sound", rest) => (Out::Heard(out.into()), rest),
                ("contact", [every, rest @ ..]) if every.parse::<f32>().is_ok() => (Out::Contact(out.into(), num(every)?), rest),
                ("contact", rest) => (Out::Contact(out.into(), 1.0), rest),
                ("burst", [at, rest @ ..]) => (Out::Burst(out.into(), num(at)?), rest),
                (_, rest) => (Out::Still(out.into(), num(how)?), rest),
            };
            match rest {
                [] => Ok((scene.into(), what, None)),
                [preset] => Ok((scene.into(), what, Some(preset.clone()))),
                _ => Err(usage.into()),
            }
        }
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
        if (v.height * scale - export.height as f32).abs() > 0.01 && v.reframes {
            // Another shape, for a scene that reframes: the same pixels a point, the preset's frame.
            let (w, h) = (export.width as f32 / v.scale, export.height as f32 / v.scale);
            if w.fract() != 0.0 || h.fract() != 0.0 {
                return Err(format!("preset {name} is {}x{} pixels, not whole points at the scene's scale {}", export.width, export.height, v.scale));
            }
            (v.width, v.height) = (w, h);
            return Ok(());
        }
        if (v.height * scale - export.height as f32).abs() > 0.01 {
            return Err(format!(
                "preset {name} is {}x{} pixels and the scene's frame is {}x{} points: not one shape (a scene is laid out \
                 for one aspect unless its Video says `reframes`)",
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

/// A thumbnail's size: `(w, h)` scaled so its longer side is `long` pixels, both sides at least 1.
fn thumbnail_size((w, h): (u32, u32), long: u32) -> (u32, u32) {
    let k = long as f32 / w.max(h) as f32;
    (((w as f32 * k).round() as u32).max(1), ((h as f32 * k).round() as u32).max(1))
}

/// RGBA8 `px` of `(w, h)` shrunk to `(sw, sh)`: each small pixel the mean of the pixels it covers.
fn shrink(px: &[u8], (w, h): (u32, u32), (sw, sh): (u32, u32)) -> Vec<u8> {
    let mut out = vec![0u8; (sw * sh * 4) as usize];
    for y in 0..sh {
        let (y0, y1) = (y * h / sh, ((y + 1) * h / sh).max(y * h / sh + 1));
        for x in 0..sw {
            let (x0, x1) = (x * w / sw, ((x + 1) * w / sw).max(x * w / sw + 1));
            let mut sum = [0u32; 4];
            for yy in y0..y1 {
                for xx in x0..x1 {
                    let at = ((yy * w + xx) * 4) as usize;
                    for (s, v) in sum.iter_mut().zip(&px[at..at + 4]) {
                        *s += u32::from(*v);
                    }
                }
            }
            let n = (y1 - y0) * (x1 - x0);
            let at = ((y * sw + x) * 4) as usize;
            for (o, s) in out[at..at + 4].iter_mut().zip(sum) {
                *o = (s / n) as u8;
            }
        }
    }
    out
}

/// `thumbs` (each `(sw, sh)`) in a grid `across` wide with a 4-pixel gutter, on mid-grey so a dark frame's edge shows.
fn tile(thumbs: &[Vec<u8>], (sw, sh): (u32, u32), across: u32) -> (Vec<u8>, (u32, u32)) {
    const GAP: u32 = 4;
    let rows = (thumbs.len() as u32).div_ceil(across).max(1);
    let (w, h) = (across * (sw + GAP) + GAP, rows * (sh + GAP) + GAP);
    let mut out = vec![96u8; (w * h * 4) as usize];
    for (i, t) in thumbs.iter().enumerate() {
        let (cx, cy) = (GAP + (i as u32 % across) * (sw + GAP), GAP + (i as u32 / across) * (sh + GAP));
        for y in 0..sh {
            let from = (y * sw * 4) as usize;
            let to = (((cy + y) * w + cx) * 4) as usize;
            out[to..to + (sw * 4) as usize].copy_from_slice(&t[from..from + (sw * 4) as usize]);
        }
    }
    (out, (w, h))
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

fn run(args: &[String]) -> Result<(), String> {
    const SR: u32 = 48_000;
    if let [what, track, out] = args
        && what == "analyse"
    {
        let bytes = std::fs::read(track).map_err(|e| format!("{track}: {e}"))?;
        let mix = music::read_wav(&bytes, track)?;
        let name = Path::new(track).file_name().and_then(|n| n.to_str()).unwrap_or(track);
        // The temp score says what it is in its name; the grid carries that, so a render can be labelled.
        let grid = music::analyse(&mix, if name.contains("temp") { "temp score" } else { name })?;
        let json = serde_json::to_string_pretty(&grid).map_err(|e| format!("{out}: {e}"))?;
        std::fs::write(out, json + "\n").map_err(|e| format!("{out}: {e}"))?;
        let picture = Path::new(out).with_extension("png");
        let size = ((mix.len() as f64 / f64::from(SR) * 30.0) as usize, 240);
        write_png(&picture, &grid.picture(&mix, size), (size.0 as u32, size.1 as u32))?;
        println!("{track}: {:.2} beats a minute, {} beats, {} bars", grid.bpm, grid.beats.len(), grid.bars.len());
        for s in &grid.sections {
            println!("  bar {:>3} at {:7.3} s  {}", s.bar, s.at, s.label);
        }
        for (k, i) in grid.impacts.iter().enumerate() {
            println!("  impact {} at {:7.3} s  {}", k + 1, i.at, grid.position(i.at).0);
        }
        println!("wrote {out} and {} (the track with its grid: faint lines beats, bright lines bars, orange lines sections)", picture.display());
        return Ok(());
    }
    if let [what, dir] = args
        && what == "score"
    {
        // The score of our own: its mix, its stems, the page, the piano roll and the loudness curve, into `dir`.
        let dir = Path::new(dir);
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let score = music::compose();
        let mix = score.mix();
        let put = |name: &str, frames: &[[f32; 2]]| {
            let path = dir.join(name);
            std::fs::write(&path, harness_native_blaze::sound::wav(SR, frames)).map_err(|e| format!("{}: {e}", path.display()))
        };
        put("music-temp-score.wav", &mix)?;
        for (name, stem) in [("stem-lead.wav", &score.lead), ("stem-harmony.wav", &score.harmony), ("stem-bass.wav", &score.bass), ("stem-drums.wav", &score.drums), ("stem-effects.wav", &score.effects)] {
            put(name, stem)?;
        }
        let mut page = score.page();
        page.push_str("\nHeard in the lead stem (written, then the pitch found, MIDI numbers; 69 is A4):\n");
        for (w, midi) in score.heard() {
            page.push_str(&format!("  bar {:>2} beat {:<4} written {} heard {midi:.2}\n", w.bar, w.beat, w.midi));
        }
        std::fs::write(dir.join("score.txt"), &page).map_err(|e| format!("{}: {e}", dir.display()))?;
        write_png(&dir.join("piano-roll.png"), &score.roll((1320, 330)), (1320, 330))?;
        write_png(&dir.join("loudness-over-time.png"), &music::loudness_curve(&mix, score.bpm, (1320, 240)), (1320, 240))?;
        print!("{page}");
        println!("wrote {}: music-temp-score.wav (temp music: our own score, a stand-in until a track is supplied), five stems, score.txt, piano-roll.png, loudness-over-time.png", dir.display());
        return Ok(());
    }
    if let [check, file, scene, name] = args
        && check == "check"
    {
        let export = preset::named(name)?;
        let mut stage = Stage::load(Path::new(scene))?;
        stage.fit(name, &export)?;
        let v = stage.video;
        let (pw, ph) = ((v.width * v.scale) as u32, (v.height * v.scale) as u32);
        let frames = (v.seconds * v.fps).round() as u32;
        print!("{}", read_back(Path::new(file), &export, (pw, ph), v.fps, frames)?);
        println!("ok: {file} is preset {name}: {}", export.words());
        return Ok(());
    }
    let (scene, out, preset_name) = parse(args)?;
    let mut stage = Stage::load(&scene)?;
    let name = preset_name.as_deref().unwrap_or(preset::DEFAULT);
    let export = preset::named(name)?;
    // A video takes its preset (the default with none named); a still or a sheet takes one only when named.
    if matches!(out, Out::Video(_)) || preset_name.is_some() {
        stage.fit(name, &export)?;
    }
    let v = stage.video;
    if let Out::Cuts = &out {
        print!("{}", stage.cuts()?);
        return Ok(());
    }
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
        eprintln!("harness-video: wrote {} ({} s at {SR} Hz, 16-bit stereo{})", path.display(), v.seconds, loudness(measured));
        return Ok(());
    }
    if let Out::Heard(path) = &out {
        let (sound, measured) = stage
            .sound(SR)?
            .ok_or_else(|| format!("{}: no BlazeMark and no Sound, so no soundtrack", scene.display()))?;
        // Ten columns a second, so a cue a tenth of a second long is a column.
        let size = ((v.seconds * 20.0) as usize, 420);
        write_png(path, &loud::picture(&sound, size), (size.0 as u32, size.1 as u32))?;
        eprintln!("harness-video: wrote {} (waveform over spectrogram, 40 Hz to 20 kHz, {} columns a second{})", path.display(), 20, loudness(measured));
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
        Out::Audio(_) | Out::Sounds | Out::Heard(_) | Out::Cuts => unreachable!("written above"),
        Out::Still(_, at) => vec![((at * v.fps).round() as u32).min(total.saturating_sub(1))],
        Out::Contact(_, every) => (0..total).step_by(((every * v.fps).round() as usize).max(1)).collect(),
        Out::Burst(_, at) => {
            let from = ((at * v.fps).round() as u32).min(total.saturating_sub(1));
            (from..(from + 10).min(total)).collect()
        }
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
    let picture = if stage.pictured { Some(backdrop::Pass::new(renderer.device(), pw, ph)?) } else { None };
    // A contact sheet's or a burst's small frames, in order.
    let mut thumbs: Vec<Vec<u8>> = Vec::new();
    let small = match &out {
        Out::Contact(_, _) => thumbnail_size((pw, ph), 192),
        Out::Burst(_, _) => thumbnail_size((pw, ph), 480),
        _ => (0, 0),
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
        // With a Backdrop, the frame is the picture with the UI over it; without, the UI alone.
        let shown = match (&picture, stage.backdrop()) {
            (Some(pass), Some(b)) => pass.draw(&b, i as f32 / v.fps, i, &tex)?.clone(),
            _ => tex.clone(),
        };
        if let Some(enc) = encoder.as_mut() {
            let target = enc.target()?;
            hdr.as_ref().ok_or("yuv: no colour pass")?.convert(&shown, &target.luma, &target.chroma)?;
            enc.encode(target, i)?;
            if i % 60 == 0 {
                eprintln!("harness-video: frame {i}/{total}");
            }
            continue;
        }
        let mut px = renderer.read(&shown);
        opaque(&mut px);
        match &out {
            Out::Video(_) => unreachable!("encoded above"),
            Out::Still(path, _) => write_png(path, &px, (pw, ph))?,
            Out::Contact(_, _) | Out::Burst(_, _) => thumbs.push(shrink(&px, (pw, ph), small)),
            Out::Sheet(dir, _) => write_png(
                &dir.join(format!("{:05}.png", (i as f32 / v.fps * 1000.0).round() as u32)),
                &px,
                (pw, ph),
            )?,
            Out::Audio(_) | Out::Sounds | Out::Heard(_) | Out::Cuts => unreachable!("written above"),
        }
    }
    if let Some(enc) = encoder {
        enc.finish()?;
    }
    if let Out::Contact(path, _) | Out::Burst(path, _) = &out {
        let across = if matches!(out, Out::Burst(_, _)) { 5 } else if small.0 > small.1 { 10 } else { 15 };
        let (sheet, size) = tile(&thumbs, small, across);
        write_png(path, &sheet, size)?;
    }
    let peak = hdr.as_ref().map_or(0.0, yuv::Yuv::peak_nits);
    let what = match &out {
        Out::Video(p) => {
            // Every render is read back: what was written is what the preset says, or the render fails.
            let report = read_back(p, &export, (pw, ph), v.fps, total)?;
            format!(
                "{} ({total} frames, {pw}x{ph} at {} fps, preset {name}: {}; brightest pixel {peak:.0} nits{}{})\nread back:\n{report}",
                p.display(),
                v.fps,
                export.words(),
                loudness(measured),
                match &stage.grid {
                    Some(g) if stage.track.is_some() => format!("; cut to {}{}", g.source, if g.source == "temp score" { " (TEMP MUSIC: a stand-in, not the film's track)" } else { "" }),
                    _ => String::new(),
                }
            )
        }
        Out::Still(p, at) => format!("{} (at {at} s)", p.display()),
        Out::Contact(p, every) => format!("{} ({} frames, one each {every} s, left to right then down)", p.display(), wanted.len()),
        Out::Burst(p, at) => format!("{} ({} consecutive frames from {at} s)", p.display(), wanted.len()),
        Out::Sheet(d, _) => format!("{} ({} frames)", d.display(), wanted.len()),
        Out::Audio(_) | Out::Sounds | Out::Heard(_) | Out::Cuts => unreachable!("written above"),
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
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args) {
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

    /// The film loads on the app's scene with its numbers from data.json, is a minute at 60 fps, fits the X preset at
    /// 1920 by 1080, has a soundtrack with a master, and shows the data file's own figure: the control changes the
    /// number in a copy of the file and the scene's constant follows.
    #[test]
    fn film_loads_from_its_data() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("scene/blaze");
        let stage = Stage::load(&dir.join("film.flecs")).unwrap_or_else(|e| panic!("{e}"));
        let mut v = stage.video;
        assert!(v.seconds == 60.0 && v.fps == 60.0 && v.reframes, "{v:?}");
        assert!(stage.pictured, "the film's root has a Backdrop");
        let x = preset::named("x-landscape-1080p60").unwrap_or_else(|e| panic!("{e}"));
        fit(&mut v, "x-landscape-1080p60", &x).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((v.width * v.scale, v.height * v.scale), (1920.0, 1080.0));
        assert_eq!(stage.mix, Some(Mix { lufs: -16.0, peak: -1.0 }));
        // It is cut to the grid: every hard cut within a frame of a beat or a half beat.
        let cuts = stage.cuts().unwrap_or_else(|e| panic!("{e}"));
        assert!(cuts.lines().count() == 12, "eleven shots, a line each under the heading: {cuts}");
        // It reframes: the upright preset is the same points a pixel, 540 by 960.
        let mut tall = stage.video;
        let upright = preset::named("x-portrait-1080x1920").unwrap_or_else(|e| panic!("{e}"));
        fit(&mut tall, "x-portrait-1080x1920", &upright).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((tall.width, tall.height, tall.scale), (540.0, 960.0, 2.0));
        let (mark, _, heard) = stage.score().unwrap_or_else(|e| panic!("{e}"));
        assert!(mark.is_some() && !heard.is_empty() && heard.len() < 20, "{} sounds: few, under the score", heard.len());
        let json = std::fs::read_to_string(dir.join("data.json")).unwrap_or_else(|e| panic!("{e}"));
        let was = data::script(&json).unwrap_or_else(|e| panic!("{e}"));
        assert!(was.contains("export const d_day_reverified_before_t = \"1,091,174\""), "{was}");
        let now = data::script(&json.replace("1091174", "1091175")).unwrap_or_else(|e| panic!("{e}"));
        assert!(now.contains("export const d_day_reverified_before_t = \"1,091,175\""), "{now}");
        // Few words: every string a scene shows, counted (a number is not a word). Under 80 for the minute.
        let mut words = 0;
        stage.world.new_query::<&harness_native_ui_text_api::Text>().each_entity(|e, t| {
            if std::iter::successors(e.parent(), |p| p.parent()).any(|p| *p.id() == stage.root) && !e.has(harness_native_ui_timeline_api::Tally::id()) {
                words += t.s.as_str().split(|c: char| !c.is_alphanumeric() && c != '-' && c != '.' && c != '/').filter(|w| w.chars().any(char::is_alphabetic)).count();
            }
        });
        assert!(words > 0 && words < 80, "{words} words on screen");
        eprintln!("the film shows {words} words");
    }

    /// A render through the X preset is what the preset says, by the file's own boxes: half a second of a small scene
    /// at 1920 by 1080 is 30 H.264 High frames of one length, BT.709, with an AAC-LC stereo track as long (silence: the
    /// scene has no sound). The controls: the same file is refused against one frame more, and against the HEVC preset.
    /// Needs the Mac's media engine, as every render does.
    #[test]
    fn x_preset_reads_back() {
        let dir = std::env::temp_dir().join(format!("harness-video-x-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        let (scene, out) = (dir.join("clip.flecs"), dir.join("clip.mp4"));
        std::fs::write(
            &scene,
            "@tree ChildOf\nclip : ui.ZStack {\n  Video: {width: 64, height: 36, fps: 10, seconds: 0.5, scale: 2}\n  Fill: $tone_ground\n  \
             badge : hn.Badge {\n    cue.FadeIn show(at: 0.1, dur: 0.3)\n  }\n}\n",
        )
        .unwrap_or_else(|e| panic!("{}: {e}", scene.display()));
        let arg = |p: &Path| p.to_str().unwrap_or_else(|| panic!("{}: not UTF-8", p.display())).to_owned();
        run(&[arg(&scene), arg(&out), "x-landscape-1080p60".to_owned()]).unwrap_or_else(|e| panic!("{e}"));
        let report = check::read(&std::fs::read(&out).unwrap_or_else(|e| panic!("{e}"))).unwrap_or_else(|e| panic!("{e}"));
        let x = preset::named("x-landscape-1080p60").unwrap_or_else(|e| panic!("{e}"));
        check::verify(&report, &x, (1920, 1080), 60, 30).unwrap_or_else(|e| panic!("{e}"));
        let audio = report.audio().unwrap_or_else(|| panic!("no audio track"));
        assert_eq!((audio.entry.as_str(), audio.channels, audio.rate), ("mp4a", 2, 48_000));
        assert!(check::verify(&report, &x, (1920, 1080), 60, 31).is_err_and(|e| e.contains("30 frames, not 31")));
        let hevc = preset::named("x-landscape-1080p60-hevc").unwrap_or_else(|e| panic!("{e}"));
        assert!(check::verify(&report, &hevc, (1920, 1080), 60, 30).is_err_and(|e| e.contains("hvc1")));
        std::fs::remove_dir_all(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    }

    /// The longest straight run of sharp change in an RGBA8 picture: along a column boundary (a vertical line) or a
    /// row boundary, the most consecutive pixels whose luma differs from their neighbour across it by more than `jump`.
    /// A change counts only when it is a step: still there between the pixels eight to either side (a thin bright line,
    /// a shape's own outline, rises and falls again within that, and is not one). A glow cut off where its shape or
    /// track ends is such a step along a line; a point, a curve, an outline or a soft edge is not.
    /// (A bright outline's halo is a little brighter eight pixels one side of where its core begins than the other,
    /// by about 20 of 255; the tests ask for 40, so a cut dimmer than that is not seen.)
    fn straight_edge(px: &[u8], (w, h): (usize, usize), jump: i32) -> (usize, String) {
        let luma = |x: usize, y: usize| {
            let at = (y * w + x) * 4;
            (i32::from(px[at]) * 3 + i32::from(px[at + 1]) * 6 + i32::from(px[at + 2])) / 10
        };
        let mut longest = (0, String::new());
        for x in 8..w - 9 {
            let mut run = 0;
            for y in 0..h {
                let step = (luma(x, y) - luma(x + 1, y)).abs() > jump && (luma(x - 8, y) - luma(x + 9, y)).abs() > jump;
                run = if step { run + 1 } else { 0 };
                if run > longest.0 {
                    longest = (run, format!("down the column x = {x}, ending at y = {y}"));
                }
            }
        }
        for y in 8..h - 9 {
            let mut run = 0;
            for x in 0..w {
                let step = (luma(x, y) - luma(x, y + 1)).abs() > jump && (luma(x, y - 8) - luma(x, y + 9)).abs() > jump;
                run = if step { run + 1 } else { 0 };
                if run > longest.0 {
                    longest = (run, format!("along the row y = {y}, ending at x = {x}"));
                }
            }
        }
        longest
    }

    /// The invariant: no light in the picture ends at a line. Every shot the kernel has, at five times through a
    /// three-second beat (its start, its middle, the fly-through at its end), has no straight run of sharp change
    /// longer than a sixth of the frame's short side (a disk's rim against its lit inside runs straight for a tenth). The control: a glow cut at the edge of its bar (the second
    /// film's fault: a beam's light stopped on a straight line where its track began) is caught.
    #[test]
    fn no_light_ends_at_a_line() {
        let (w, h) = (480usize, 270usize);
        let limit = h / 6;
        // The control, drawn by hand: a soft bar whose glow is cut off at x = 120.
        let mut cut = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 120..w {
                let v = (255.0 * (-((y as f32 - 135.0) / 30.0).powi(2)).exp()) as u8;
                cut[(y * w + x) * 4..(y * w + x) * 4 + 3].copy_from_slice(&[v, v, v]);
            }
        }
        assert!(straight_edge(&cut, (w, h), 40).0 > limit, "the control's cut glow was not seen");
        let renderer = Renderer::new(objc2_metal::MTLCreateSystemDefaultDevice().unwrap_or_else(|| panic!("no Metal device")));
        let pass = backdrop::Pass::new(renderer.device(), w as u32, h as u32).unwrap_or_else(|e| panic!("{e}"));
        for shot in 0..=17 {
            for t in [0.05f32, 0.6, 1.5, 2.4, 2.95] {
                let b = backdrop::Backdrop { shot: shot as f32, at: 10.0, a: if shot == 11 { 5.0 } else { 1.0 }, b: 1.2, c: 2.0, d: 0.0, e: 0.0, f: 3.0 };
                let tex = pass.picture(&b, 10.0 + t).unwrap_or_else(|e| panic!("{e}"));
                let px = renderer.read(tex);
                let (run, place) = straight_edge(&px, (w, h), 40);
                assert!(run <= limit, "shot {shot} at {t} s: light ends at a straight line {run} pixels long (over {limit}), {place}");
            }
        }
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
