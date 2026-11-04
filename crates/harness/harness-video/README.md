# harness-video

Renders a video from a flecs scene, on the harness app's own stack: the `ui.*` prims, layout, motion and draw passes,
and the shared Metal renderer and paint pass (`harness-native-render`). Frame `i` is drawn at `i / fps` seconds of a
fixed clock (never the wall clock), so the same scene gives the same pixels every run, and any frame can be drawn alone.

    cargo run --release -p harness-video -- harness-video/scene/launch.flecs blaze-launch.mp4   # the video
    cargo run --release -p harness-video -- harness-video/scene/launch.flecs still.png 6.0      # one frame
    cargo run --release -p harness-video -- harness-video/scene/launch.flecs frames/ 0.5        # a frame each 0.5 s
    cargo run --release -p harness-video -- harness-video/scene/launch.flecs sound.wav          # the soundtrack alone

The mp4 is H.264 High, yuv420p, BT.709, `+faststart` (what X/Twitter, Slack and browsers play), encoded by `ffmpeg`
from PATH, with the soundtrack as AAC.

## A scene

- The root node carries `Video: {width, height, fps, seconds, scale}`: the frame in points, drawn at `scale` pixels a
  point (`960 x 540` at 2 is 1920 x 1080).
- `tokens.flecs` beside the scene holds the names the prims read (tones, radii, sizes, springs, and the `Strong`,
  `Ink`, `Muted`, `Accent`, `Wash`, `Plate` prefabs); it loads before the prims and the scene.
- Motion is data. A `Cue` child moves one member of its node, named by path, over a stretch of the clock:
  `{ Cue: {member: "Pose.fade", from: 1, to: 0, at: 0.5, dur: 0.4, ease: Out} }`. Any numeric member of any reflected
  component can be cued: `Size.w.value` relays out the neighbours, `Pose` (`dx`, `dy`, `shrink`, `fade`, `cut`) moves,
  scales, fades or types on only the node and what is under it. Cues on one member take turns by `at`. Templates name the
  moves a video makes (`cue.FadeIn show(at: 2.0)`; see `scene/launch.flecs`).
- `BlazeMark: {start, exit}` is blaze's wordmark set on fire, live (`harness-native-blaze`, ported from
  indexable-inc/anim): its letters drop in from the top of the frame at `start`, the fire catches, and from `exit` the
  letters burn into embers. Its soundtrack (the landings climbing to A major, the physically modelled fire, the burn) is
  synthesised offline from the same times and is the video's sound; a scene without a `BlazeMark` is silent.
- `Picture: {handle}` with `Pixelated` draws pixel art from a PNG under the scene's directory; `Frames: {count, fps}`
  plays a sheet of frames stacked downwards. Pictures and the mark are textures whose texels are fetched, never
  filtered: crisp, with no seam between cells, at any size or mid-animation scale.

`scene/launch.flecs` is blaze's launch video.
