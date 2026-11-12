# harness-video

Renders a video from a flecs scene, on the harness app's own stack: the `ui.*` prims, layout, motion and draw passes,
and the shared Metal renderer and paint pass (`harness-native-render`). Frame `i` is drawn at `i / fps` seconds of a
fixed clock (never the wall clock), so the same scene gives the same pixels every run, and any frame can be drawn alone.

    harness-video SCENE.flecs OUT.mp4 [PRESET]        # the video, as an export preset, then read back and checked
    harness-video SCENE.flecs still.png 6.0 [PRESET]  # one frame
    harness-video SCENE.flecs sheet.png contact [PRESET]   # a contact sheet: a small frame a second, one image
    harness-video SCENE.flecs burst.png burst 6.0 [PRESET] # ten consecutive frames from 6.0 s, one image
    harness-video SCENE.flecs frames/ 0.5             # a frame each 0.5 s
    harness-video SCENE.flecs sound.wav               # the soundtrack alone, as it is in the video (16-bit, 48 kHz)
    harness-video SCENE.flecs sounds                  # every sound of the soundtrack, in time order
    harness-video check OUT.mp4 SCENE.flecs PRESET    # read a finished mp4 back and hold it to the scene and preset

(`blaze run --release -p harness-video -- ...` from `crates/harness`.) The mp4 is made natively with no ffmpeg
(`src/encode.rs`): VideoToolbox on the media engine (hardware required; there is no software path) into pixel buffers
the GPU wrote, and AVAssetWriter with the moov first. No frame is copied: `src/yuv.rs` writes each frame's Y and CbCr
straight into the encoder's own IOSurface-backed pixel buffer, mapped into Metal by CoreVideo. Stills and frame sheets
are 8-bit sRGB PNG, clipped to SDR.

## Export presets

`presets.json` is the one home of every encoder setting, by name (`src/preset.rs` is its typed form and refuses a field
it does not know). It also records what X's upload page says, quoted, with the date it was fetched and what is not
established.

| preset | what |
| --- | --- |
| `x-landscape-1080p60` | X, subscribed accounts: H.264 High, 1920 x 1080, 60 fps, 4:2:0 8-bit, BT.709 video range, 40 Mb/s target, a key frame a second, closed GOP, AAC-LC 256 kb/s stereo 48 kHz |
| `x-landscape-720p60` | X, unsubscribed accounts: the same at 1280 x 720 |
| `x-square-1080`, `x-portrait-1080x1920` | X's other shapes, for a scene whose `Video` says `reframes` (else a preset whose aspect is not the scene's is refused) |
| `x-landscape-1080p60-hevc` | HEVC Main 8-bit; unverified on X (its page names H.264 only) |
| `youtube-hdr10-master` | with no preset named: H.265 Main 10, constant quality 0.85, HDR10 (PQ over BT.2020, the mastering display P3-D65 at 1000 nits as SEI), a half-second GOP, ALAC 24-bit, at the scene's own size and rate (`scene/launch.flecs`: 7680 x 4320) |

A preset's width over the scene's width in points is the scale it is drawn at, and its `fps` replaces the scene's (a
timeline is in seconds: frame `i` is drawn at `i / fps`, so no frame is dropped or doubled at any rate). For HDR10 the
renderer's extended range (1.0 is SDR white) puts SDR white at 203 nits (BT.2408) and what is drawn brighter goes past
it; an SDR preset clips at white and keeps the app's sRGB values, tagged BT.709 as web video is.

Every render is read back (`src/check.rs`, the file's own boxes, no decoder): the moov is before the mdat; the codec,
profile and size are the preset's; there are exactly seconds x fps frames, each the same length, each shown in its own
slot once; a key frame at least as often as the preset says; the colour tag; and the audio's codec, rate, channels and
length against the video's. A file that differs fails the render, listing how.

## A scene

- The root node carries `Video: {width, height, fps, seconds, scale}`: the frame in points, drawn at `scale` pixels a
  point (`960 x 540` at 2 is 1920 x 1080).
- A scene is loaded into the app's own world (`harness_native::scene::world`), so it names the app's tokens
  (`harness-native/scene/00-tokens.flecs`: `$tone_ground`, `$size_body`...), the `ui.*` prims and the app's `hn.*`
  widgets (`hn.Badge`, a chat's bubble, a tree row) exactly as the app does: a video of the app is made of the app's
  parts, and nothing is copied. What is only the video's (its own colours and sizes) goes in a token file beside the
  scene that the scene includes (`include tokens`; `scene/tokens.flecs` is the launch video's).
- Motion is data. A `Cue` child moves one member of its node, named by path, over a stretch of the clock:
  `{ Cue: {member: "Pose.fade", from: 1, to: 0, at: 0.5, dur: 0.4, ease: Out} }`. Any numeric member of any reflected
  component can be cued: `Size.w.value` relays out the neighbours, `Pose` (`dx`, `dy`, `shrink`, `fade`, `cut`) moves,
  scales, fades or types on only the node and what is under it. Cues on one member take turns by `at`. The `cue.*`
  templates name the moves (`cue.FadeIn show(at: 2.0)`, `cue.FadeOut`, `cue.PushIn`), shared with every host in
  `harness-native-ui-prims/scene/cue.flecs`; a move a scene needs twice goes there.
- `BlazeMark: {start, exit, glow}` is blaze's wordmark set on fire, live (`harness-native-blaze`, ported from
  indexable-inc/anim): its letters drop in from the top of the frame at `start`, the fire catches, and from `exit` the
  letters burn into embers. Its soundtrack (the landings climbing to A major, the physically modelled fire, the burn) is
  synthesised offline from the same times and is the video's sound; a scene without a `BlazeMark` is silent. `glow` takes it past white: the
  flames, white-hot letters and embers rise by their heat to `glow` SDR whites (4.93 in `launch.flecs`: the 1000-nit
  HDR10 peak over 203-nit white), drawn as a light layer over the mark; the UI stays at white.
- Sound is data, one component per concern: `Sound: {voice}` (`Bell`, `Mallet`, `Bass`, `Click`, `Thud`, `Whoosh`,
  `Fire`, `Pad`, `Rise`), `Onset: {at}`, `Lasts: {dur}`, `Pitch: {hz}`, `Gain: {amp}`, `Pan: {pos, sweep}` (-1 the frame's left edge,
  1 its right; `sweep` moves it). `harness-native-ui-prims/scene/voice.flecs` has a `voice.*` prefab per voice with its
  defaults, so an instance sets only its onset and what differs:
  `ding : voice.Bell {` then `Onset: {at: 2.0}` and `Pitch: {hz: 1760}` on their own lines, `}`. Only sounds under the frame (the node with the `Video`) are the video's. They mix in the same room as
  the mark's score, and every voice, the mark's included, is placed binaurally (a spherical head: the delay around it
  and its shadow on the far ear), so on headphones a sound comes from where it is on screen; on speakers it is an
  ordinary stereo mix.
- Words: `GlyphsIn: {at, per, dur, by, ease}` brings them in glyph by glyph (each rises and fades, `per` seconds after
  the one before); `Highlight: {file}` colours them by their grammar (tree-sitter; shell so far) in the `code_ink.*`
  tokens; `Caret: {at, until, blink, ink}` puts a block caret after the last glyph shown (pair it with a `Pose.cut`
  cue to type them on). Videos set every character as its own glyph (no ligatures).
- `Picture: {handle}` with `Pixelated` draws pixel art from a PNG under the scene's directory; `Frames: {count, fps}`
  plays a sheet of frames stacked downwards. Pictures and the mark are textures whose texels are fetched, never
  filtered: crisp, with no seam between cells, at any size or mid-animation scale.

- The picture is shaders: a root with `Backdrop: {shot, at, a, b, c, d, e, f}` is drawn over a full-frame image a
  Metal compute kernel makes from those numbers each frame (`src/backdrop.rs`: a race of comets, a graph a ring runs
  through, a core in orbit, a swarm thinning, a beam, points gathering), the UI laid over it, then grain and a
  vignette. Cue its members (a zero-length cue sets one from a beat's start) and the picture is data too.
- `Video: {..., reframes: true}` says the scene is centre-weighted and sized in shares of the frame: a preset of
  another shape (`x-portrait-1080x1920`) then redraws it upright at the same pixels a point, instead of being refused.
- Numbers are data: a `data.json` beside the scene becomes constants before the scene loads (`src/data.rs`): each leaf
  is `d_<keys joined by _>`; a number also has `_t`, its words (digits grouped by three, then its object's `unit`); a
  range `{"low", "high"}` has `_low` and its words; an object with `"compare": [names]` gives each name a `_share`, its
  value over the largest (a bar's length). A scene names a figure (`Text: {s: $d_rows_noop_blaze_t}`) and never types one.
- `Tally: {value, decimals, unit}` shows a number as a node's words; cue `Tally.value` and it counts (`cue.Count`,
  `cue.Fall`). `cue.Widen` grows a bar, `cue.CutIn`/`CutOut` are a shot's window, `cue.Arrive`, `cue.Lift` and
  `cue.Type` bring words in.
- `Mix: {lufs, peak}` on the root masters the soundtrack (`src/loud.rs`): brought to `lufs` integrated (ITU-R BS.1770,
  K-weighted and gated) with no true peak over `peak` dBTP, by a look-ahead limiter; the render prints what it then
  measures. `voice.Pad` is a bed under a stretch and `voice.Rise` a rise into a hit.

`scene/launch.flecs` is blaze's launch video (the mark alone, 10 seconds). `scene/blaze/film.flecs` is the film for X,
landscape and upright from one scene: its storyboard is at its top, each beat is a file beside it, `cuts.flecs` is when
each starts, and every number it shows is a row of `scene/blaze/data.json`. How to make one, and what it is judged by:
the `making-videos` skill (`.agents/skills/making-videos`).

    harness-video harness-video/scene/blaze/film.flecs blaze-film.mp4 x-landscape-1080p60
    harness-video harness-video/scene/blaze/film.flecs blaze-film-tall.mp4 x-portrait-1080x1920
