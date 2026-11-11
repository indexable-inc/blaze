# harness-video

Renders a video from a flecs scene, on the harness app's own stack: the `ui.*` prims, layout, motion and draw passes,
and the shared Metal renderer and paint pass (`harness-native-render`). Frame `i` is drawn at `i / fps` seconds of a
fixed clock (never the wall clock), so the same scene gives the same pixels every run, and any frame can be drawn alone.

    blaze run --release -p harness-video -- harness-video/scene/launch.flecs blaze-launch.mp4   # the video
    blaze run --release -p harness-video -- harness-video/scene/launch.flecs still.png 6.0      # one frame
    blaze run --release -p harness-video -- harness-video/scene/launch.flecs frames/ 0.5        # a frame each 0.5 s
    blaze run --release -p harness-video -- harness-video/scene/launch.flecs sound.wav          # the soundtrack alone

The mp4 is a YouTube HDR master, made natively with no ffmpeg (`src/encode.rs`): H.265 Main 10 encoded by VideoToolbox
on the media engine (hardware required; there is no software path), constant quality 0.85, 4:2:0 10-bit, HDR10 (PQ over
BT.2020, the mastering display P3-D65 at 1000 nits as SEI), a half-second GOP, tagged `hvc1`, written by AVAssetWriter
with the moov first, and the soundtrack lossless: Apple Lossless (ALAC), 24-bit stereo at 48 kHz (YouTube re-encodes
the audio anyway, so the master keeps all of it; Apple's AAC encoder also tops out at 320 kb/s for 48 kHz stereo). No frame is copied: `src/hdr.rs` writes each frame's Y and
CbCr straight into the encoder's own IOSurface-backed pixel buffer, mapped into Metal by CoreVideo. The renderer draws
extended range (sRGB-encoded half floats; 1.0 is SDR white), and `src/hdr.rs` puts SDR white at 203 nits (BT.2408), so the UI looks as
it does in SDR and only what is drawn brighter than white (the fire, white-hot letters) goes past it. Stills and frame
sheets stay 8-bit sRGB PNG, clipped to SDR. `scene/launch.flecs` is 960 x 540 points at scale 8: 7680 x 4320, 60 fps.

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
  `Fire`), `Onset: {at}`, `Lasts: {dur}`, `Pitch: {hz}`, `Gain: {amp}`, `Pan: {pos, sweep}` (-1 the frame's left edge,
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

`scene/launch.flecs` is blaze's launch video.
