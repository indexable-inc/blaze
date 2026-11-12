---
name: making-videos
description: "Making a video with the repository's own renderer (crates/harness/harness-video): a film is flecs script scenes over full-frame shaders, numbers bound from one data file, sound synthesised from events, exported through named presets and read back. Covers the pipeline and its commands, the craft rules a film is judged by (very few words, short captions that pop in and out, shaders not slides, a hook, a new picture every few seconds, legible with sound off, reframing for upright), the honesty rule for comparisons, platform upload specs with source and date, how to review your own render (contact sheet, bursts, phone size, loudness), and the traps. Load before making or changing any video, launch clip or animated demo, before touching crates/harness/harness-video, and before writing a scene under harness-video/scene/."
---

# Making videos

A video here is data rendered by `crates/harness/harness-video` (its README is the reference): flecs script scenes on
the app's own stack, a picture drawn by shaders under them, every number from one JSON file, sound synthesised from
events in the scenes, encoded by VideoToolbox, no ffmpeg anywhere. Two films made this way were rejected by the owner.
The first was "a deck, not a film": slides of headlines, a labelled bar chart, a footnote. The second had shaders and
few words and was still wrong: "a lot of small statistics but not the bigger picture", nine beats of numbers and
two images used over and over, and a glow that ended on a straight line. The rules below are what those cost. Read
them before the first line of a scene.

## Before building: what is it, and three treatments

- **A trailer is ideas, not metrics.** Say what the thing is and will be (for blaze: a compiler for agents, its own
  capability-based standard library, linear types, a deterministic simulation, proofs in Lean, enforced effects,
  sandboxing, byte-identical output, code identity, then what is coming). Speed is one or two beats, the strongest
  measured rows only. If most beats are a number, it is a dashboard.
- **Verify every claim in the tree before it is a beat**, and write beside the beat in the storyboard the file and line
  that backs it. What is designed or on a branch goes under a "soon" tag. Use the tool's own words for an error
  (grep the message), never a paraphrase.
- **Write three one-paragraph treatments** (art directions that could not be mistaken for each other: one continuous
  flight through a world; flat colour fields and hard typographic cuts; the code itself as the material), pick the one
  most likely to be rewatched, say why, and commit to it. Do not start from the last cut's parts.
- **High-entropy first.** The first two seconds hold the most surprising concrete claims in the film, a hard cut
  each, under a second each; the name comes after. Context follows surprise.
- **Open a loop early and close it late** (the galaxy small in the sky behind the name; the film arrives in it at
  the peak).

## The pipeline

From `crates/harness` (build: see Traps), with `F=harness-video/scene/<film>/film.flecs`:

| Want | Command |
| --- | --- |
| The video, read back and checked | `harness-video $F out.mp4 x-landscape-1080p60` |
| The same scene upright | `harness-video $F tall.mp4 x-portrait-1080x1920` (the root's `Video` says `reframes: true`) |
| A contact sheet: a frame each EVERY seconds (1), one image | `harness-video $F sheet.png contact [EVERY] [PRESET]` |
| The mix as a picture (waveform over spectrogram) | `harness-video $F sound.png sound` |
| Ten consecutive frames from a time, one image | `harness-video $F burst.png burst 14.9 [PRESET]` |
| One full frame | `harness-video $F still.png 14.9 [PRESET]` |
| The mix alone (with its loudness printed) | `harness-video $F mix.wav` |
| Every sound event, in time order | `harness-video $F sounds` |
| Check a finished file against a preset | `harness-video check out.mp4 $F PRESET` |
| A track's beat grid, and its picture | `harness-video analyse music.wav beats.json` |
| Every hard cut, held to the grid | `harness-video $F cuts` |
| Our own score: mix, stems, page, piano roll, loudness curve | `harness-video score DIR` |

- **Presets** are data: `harness-video/presets.json`, one home for codec, profile, pixel format, bitrate, key frames,
  colour, audio and container. Add a platform by adding a preset there, never a flag.
- **Every render is read back** (`src/check.rs`): frame count = seconds x fps, one frame a slot, the codec, profile,
  size, colour tag, audio codec, rate and length. A render that differs fails. Then check the bitrate yourself:
  file bytes x 8 / seconds, against the platform's floor.
- **A film's files** (`scene/blaze/` is the worked example): `film.flecs` (the storyboard as comments, the root with
  `Video`, `Backdrop`, `Mix`, the bed), `cuts.flecs` (when each beat starts: the one place to retime), a file a beat
  (its picture's numbers as cues on `Backdrop`, its captions, its sounds), `tokens.flecs`, `parts.flecs`, `data.json`.
- **The picture** is `src/backdrop.rs`: Metal compute kernels (MSL compiled at start-up, like `yuv.rs`) that draw a
  shot from the root's `Backdrop: {shot, at, a..f}`, then lay the UI over it with grain and a vignette. A new look is
  a new `shot` there; a beat picks it and feeds it numbers with zero-length `Cue`s (`from` = `to`, `dur: 0`).
- **Motion** is `Cue`s on any numeric member (`cue.*` templates in `harness-native-ui-prims/scene/cue.flecs`:
  `CutIn`/`CutOut` a window, `Arrive`/`Pop`/`Leave` a caption, `Count`/`Fall` a `Tally`, `Widen` a bar).
- **Sound** is `voice.*` events in the beat's file (`Onset`, `Lasts`, `Pitch`, `Gain`, `Pan`), synthesised
  deterministically by `harness-native-blaze`'s `sound`, mastered to the root's `Mix: {lufs, peak}` (`src/loud.rs`).

## Pace: hold, one caption, the reading test

The third cut had 22 cuts in a minute and was rejected: "goes through things way too fast". An idea the viewer cannot
say back was not shown.

- **Nine to eleven shots a minute**, 7 or 8 ideas. An idea gets 5 to 8 s (two bars at 88): an entrance of about a
  second, a HOLD where the image is readable and the caption sits still, the image doing its one thing, and an exit
  that hands motion to the next shot. Fast cutting only for a bar or two before a drop. One near-still moment before
  the peak. The end card holds 4 s.
- **One caption at a time, still for at least 1.5 s**; none on screen under 1.2 s. A viewer must be able to read it
  twice. A caption arrives on a beat, a beat or two after its image, and leaves a tenth of a second before the cut.
- **The reading test**: for each shot write, in the storyboard, the sentence a first-time viewer would say back
  ("build scripts are locked in"). If you cannot write it, or two shots have the same one, cut or lengthen.

## The score: one motif

If the film has no licensed track, the score is composed, not assembled from effects (`src/music.rs` `compose`; the
fourth cut's is the worked example). The owner's words: it "builds up to something, has a motif, and the final thing
is slightly different".

1. **One motif**: 3 to 5 notes, singable, a clear rhythm and contour, stated in note names (E4 A4 G4 E4: up a fourth,
   a step back, home: a question).
2. **Form**: introduce it bare and quiet; restate it over a progression that wants to resolve (i VI III VII), adding a
   layer at a time (pad, bass, pulse, a counter-line that answers); build (rhythm thickens, register rises, a riser);
   a bar of near-silence; the drop (the motif in octaves, everything); a final statement that is the motif CHANGED
   (E4 A4 B4 C#5: it rises to the major third instead of falling back, twice as slow, over the major chord).
3. **Palette, small**: a plucked or bell lead, a soft pad, a warm bass, kick and hat, one riser, one low hit, a
   Schroeder reverb, an echo on the lead. Voice-lead the chords. Sound effects few and under the music.
4. **Check it on paper, since you cannot hear it** (`harness-video score DIR` writes all of these): the page (key,
   tempo, the motif, each bar's chord and voices), the piano roll against the bar lines, the loudness curve (it must
   climb, dip before the drop, and stand tallest on it), the spectrogram, the stems (lead, harmony, bass, drums,
   effects) for someone who can hear, and the test that the pitches heard in the lead stem are the written notes and
   that the final statement differs from the first exactly as designed.
5. **Say what it is**: a score synthesised by an agent that cannot hear is the weakest part of a film. A licensed track
   or a composer does better; the pipeline re-cuts to any WAV.

## Cut to music, in musical time

A trailer is edited to a track, frame in, frame out. Author cuts as bar and beat, never as seconds.

1. **The track** is a file the owner supplies with the rights to use it, as `music.wav` beside the scene (WAV: 16, 24
   or 32-bit PCM or float, any rate; it is not committed). **Never download or rip a recording**, and never imitate a
   recognisable part of one. Until a track is supplied, `harness-video score DIR` writes our own score (above) to cut
   against; a render made with it says TEMP MUSIC and is named so. With a slow pace prefer a track with a clear build.
2. **The grid**: `harness-video analyse music.wav beats.json` (`src/music.rs`: spectral-flux onsets, tempo by
   autocorrelation, a dynamic-programming beat tracker fitted to one even tempo, downbeats from the low band, sections
   from each bar's energy, the strongest impacts). It also writes `beats.png`: the track with faint lines on beats,
   bright lines on bars, orange lines on sections. **Look at it**: the bright lines must sit on the kicks. A grid at half
   or double tempo, or counted from the wrong beat, moves every cut.
3. **Placing cuts**: `beats.json` becomes `m_*` constants (`$m_b15_1` bar 15 beat 1, `$m_b15_1h` the half beat after,
   `$m_drop`, `$m_impact_1`, `$m_beat` a beat's seconds), each snapped to a frame. `cuts.flecs` is nothing but those;
   inside a beat, offsets are whole or half beats (`$t_rings + 4 * $beat`). Another track re-times the film.
4. **The check**: `harness-video $F cuts` lists every hard cut with its bar and beat and fails on one more than a frame
   off the grid (the film's test runs it).
5. **Where cuts go**: on the beat for an impact (the image and the hit arrive together); a caption or a camera move
   may start a few frames before a beat so it lands on it; things leave just before the next beat, so the beat belongs
   to what arrives. Motion carries through a cut (the fly-through that ends one landmark is the approach to the next).
6. **The shape**: a cold open of hard cuts on single hits; a build whose cut rate doubles each phrase, with a riser;
   a bare beat before the drop; the hero image on the drop's downbeat, held; a montage; a break (the film's one
   silence) with one line on one hit; the mark on the final downbeat; a button a bar later.
7. **The mix**: the track is the hero (`Music: {gain, under}` on the root mixes the scene's own sounds under it);
   the loudness target does not change.

## Numbers are bound, never typed

A `data.json` beside the scene becomes `d_*` constants before the scene loads (`src/data.rs`): `d_rows_noop_blaze`
(the number), `..._t` (its words with unit), `..._share` (its value over the largest of the object's `compare` list:
a bar's or a lane's length), ranges as `{"low", "high"}`. A scene writes `$d_rows_noop_blaze`, never `11.9`. The test
is: change the file, render, and the film shows the new number. Each row carries `source` and `conditions`.

## Craft: what a film is judged by

1. **Almost nothing to read.** At most 2 to 5 words on screen at once; under 80 words in a minute, counted (the
   film's test counts them). One line of real code or the compiler's own error may be an image when it is the point. No paragraphs, bullet cards, "5 / 7", legends, footnote blocks. If it needs a sentence,
   find the picture that says it.
2. **Captions pop in and out.** One phrase at a time, bold, low-centre or beside the thing it names, 0.6 to 1.5 s
   each (longer only under a race it labels), in with a quick scale (`cue.Arrive` + `cue.Pop`), gone (`cue.Leave`)
   before the next. The word that matters may take the accent. At 2 pixels a point, 34-point bold reads on a phone;
   19-point is the floor for a label; the conditions line is the only smaller thing and it fades.
3. **Numbers are the heroes.** Big, few, moving: a `Tally` that counts while the picture changes with it.
4. **Shaders, not rectangles.** Every beat is a living image with depth: signed-distance shapes, layers of points
   the camera moves through, glow, grain, a vignette; near-black, one accent, which may invert for an interrupt. A
   plate with a headline on it is a slide; so is a bar with a label.
   **Every falloff reaches zero inside what is drawn.** A glow is a Gaussian of a distance to a point, a segment or
   a curve and nothing else: never multiply it by a `step` at a track's start, a shape's box or a quad's edge, or its
   light ends on a straight line (the second film's beam did, at its left end). `no_light_ends_at_a_line` draws
   every shot and fails on a long straight run of sharp change; add a shot and it is checked.
5. **A hook in the first 2 s** (one striking image and one claim, not a barrage), no title card. **Each shot a
   different image and a different idea, held (see Pace): no image twice.** A pattern interrupt every 5 to 8 s (the palette turned over, a hard cut to one line of
   code or an error, a freeze, a fly-through). One sustained peak around two thirds through (the rise in the sound
   lands on it). What is coming as a fast montage. An end card (mark, name, repository) under 3 s. A minute is 60.0 s.
6. **Sound off first.** Platforms autoplay muted: the picture must carry everything. Sound follows the picture (a
   whoosh on a streak, a hit on a finish, a rise into the peak) and never explains.
7. **Reframe.** Most short video is watched upright. Compose centre-weighted, size things as shares of the frame
   (`Percent`), keep shader geometry in units of the short side, and render both shapes from the one scene.
8. **Comparisons are beats, not tables.** One comparison, 2 to 4 s: things racing or a quantity shrinking, numbers
   ticking, two or three words. Draw to scale: a lane's time is its share of the row's slowest.

## Honesty (claims from evidence)

A comparison shown is a measured row of the data file, drawn to scale, with its conditions on screen once (program,
machine, date). A row is shown as measured or left out: never altered, rounded in our favour, cropped to a win, or
implied. Where blaze loses and the row is shown, the caption says so ("Relax leads", "still behind"). Name a
competitor only as the data file describes it. No "fastest". A design claim must be something the docs say is built;
anything else is labelled "in progress" or is not in the film.

## Platform specs (re-fetch before relying on them)

- **X**, from https://docs.x.com/x-api/media/quickstart/best-practices, fetched 2026-11-11 (quoted in
  `presets.json`): H.264 High, 4:2:0, progressive, closed GOP, at most 60 fps, AAC-LC mono or stereo (HE-AAC refused),
  video at least 5,000 kbps and audio at least 128 kbps, aspect between 1:3 and 3:1, 1080p only for subscribed
  accounts (720p otherwise), 8 GB and 20 minutes for a post. Not on the page, so unverified: HEVC, 10-bit, HDR, the
  container, anything over 1080p. The consumer help page answered 403.
- **YouTube**: the `youtube-hdr10-master` preset (HEVC Main 10, HDR10, ALAC); its limits are not re-verified here.
- **Any other platform**: fetch its page, quote it with the date in `presets.json`, add a preset, mark what the page
  does not say as unverified.
- **Loudness**: -16 LUFS integrated, true peak at or under -1 dBTP (`Mix: {lufs: -16, peak: -1}`); the render prints
  what it measured. Nothing that carries meaning under 80 Hz (phone speakers).

## Review your own render (before anyone else sees it)

1. Contact sheet, landscape and upright. Read it as a viewer: is any frame a slide? Two neighbouring seconds the
   same picture (dead air)? A caption cut off, or sitting on a glare?
2. Bursts across each caption's entrance and each finish: does it pop, is it legible mid-motion, does a flash hide a
   number?
3. Phone size: one upright still shrunk to about 400 pixels wide (`sips -Z 800`). Can you read every word?
4. Count the words. Check the bitrate. Read the loudness line. Run `sounds` and check the first second is not silent.
   You cannot hear the mix: render `sound.png sound` (waveform over spectrogram) and look for dead stretches, a
   waveform touching its top, a silence you did not mean; list the cue times for the owner to spot-check.
5. Contact sheet at a frame each half second (`contact 0.5`), and a burst over every cut in the first five seconds.
6. Write the critique down: would I keep watching at 2 s, 5 s, 15 s, 40 s? Which one frame would someone screenshot?
   What would make an engineer quote-post it? If an answer is "nothing", it is not done.
7. Open the images with your image reader and look. A film nobody looked at ships its bugs: the first cut of this
   one had every lane label gone after 0.3 s and nobody would have known from the read-back.

## Traps

- **Building with today's blaze** (tried 2026-11-11 with `/tmp/blaze-inst/blaze-engine`, a cargo-built blaze of the
  day's tree, own `BLAZE_HOME` under `/private/tmp`): without `--ignore-pin` it fetches and runs the pinned release,
  which cannot read the manifest; a daemon that run started then refuses the newer client ("speaks API schema 40,
  this blaze 50") until it is stopped. With `--ignore-pin` and that daemon gone it builds, slowly from a cold home
  (20 minutes cloning flecs, then 1,776 of 2,567 units in the 25 minutes it was given, no error): give it an hour
  and nothing else to do. `env BLAZE_HOME=/private/tmp/<own> BLAZE_DAEMON_MEMORY_GB=48 <blaze-engine> --ignore-pin
  build --release -p harness-video` from `crates/harness`, with the manifest as committed.
- **The build.** cargo cannot read the harness workspace (`harness-native-render` depends on
  `harness-native-shaders` with `artifact = "gpu"`, a blaze extension), and the pinned blaze (0.20261105.4) predates
  that. The proper build is `blaze build --release -p harness-video` with a blaze from after 2026-11-10. Until the pin
  moves, the film lane built with cargo and the pinned nightly through a local, uncommitted edit of that one
  dependency line plus `BLAZE_GPU_MSL_HARNESS_NATIVE_SHADERS` pointing at an MSL file (Kip's shaders; a film draws no
  Kip). Say so in your report; do not commit the edit (it breaks blaze's build).
- **One target directory.** Build into main's warm target (`CARGO_TARGET_DIR=/Volumes/Projects/blaze/crates/harness/target`)
  and copy the binary out before running it: the build replaces main's `target/release/harness-video`.
- **`Cue`s on one member take turns**: the latest one begun holds the value, across beats. A beat must set every
  `Backdrop` member it reads at its own start, or it inherits the last beat's.
- **A `BlazeMark` rings every `GlyphsIn`** in the scene as a bell. Use `cue.Arrive`/`cue.Pop` for words.
- **Metal's shading language**: `half` is a type name; file-scope constants are `constant float3 X = ...`, not
  `static const`. A kernel that does not compile fails at start-up with Metal's message.
- **A grid of points can only glow as far as its 3 by 3 neighbourhood**: a wider falloff shows as squares.
- **Flecs script**: reserved words (`in`, `new`, `with`...) cannot name an entity; a script may read at most 64
  distinct tokens, so split a beat-heavy scene into a file a beat; `include` comes first in a file, so shared times
  live in an included file of `export const`s.
- **An api crate's test cannot hold a `ScriptString`** (it needs flecs's allocator): test in `harness-video`.
- **AVAssetWriter leaves `<name>.mp4.sb-*` beside its output** for a while; remove them before listing deliverables.
- **Never post or upload.** Deliver files and the report; the owner posts.
