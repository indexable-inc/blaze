---
name: flecs-ui
description: "Building native UI as Flecs (the harness app in crates/harness/): views are flecs script prefabs over den's primitives (ui.Row/Column/Grid/LazyColumn/IconButton...), state is components and relationships, Rust only feeds data in and runs the hot paths. Covers the layering, the primitive vocabulary, tokens and themes, keybindings as data, how to verify with offscreen shots, the Flecs 4.2 traps, and the 4.2 reactive-template style views are moving to. Load before touching any crates/harness/**/*.flecs, the crates/harness/harness-native-ui-* crates, or adding a widget, row, pane or shortcut."
---

# Native UI as Flecs

General Flecs rules, traits and script syntax are in the `flecs` skill; this one is how the harness app builds UI
with them. Paths below are relative to `crates/harness/`. The app is copied from den's native stack: flecs v4.2.0
(the indexable-inc/flecs fork, branch `indexable/v4.2`, pinned by rev in `crates/harness/vendor/hyperion-flecs-c/Cargo.toml`), Metal through the
indexable-inc/slang fork (`harness-native/build.rs`), den's text shaper plus Slug outlines, libghostty-vt for terminal
state (`harness-vt`).

## Layers (each knows only the ones below)

1. **Model** (`harness-native-model/src/lib.rs`): the world's domain. `Device` roots; `Object` entities with exclusive
   `(Kind, Claude|Codex|CursorAgent|Shell|Proc|Stage)` and `(Status, Running|Exited|Killed|Lost|Pending|Skipped)`,
   `Name`, `Title`, `Cwd`, `ExitCode`, `Created`/`Ended`, `Term`; runs carry `Run`, stages `StageNo`; flecs `ChildOf`
   for the tree; tags such as `Archived`, `Closed`, `Pinned`. The daemon feeds it (`DaemonInbox` system); auto-archive
   is the `ArchiveRule` system here.
2. **Primitives** (`harness-native-ui-prims/scene/prims.flecs` + the `harness-native-ui-{layout,motion,draw,input}`
   crates and their `-api` crates): the vocabulary every view is made of. Never draw ad hoc in Rust when a primitive
   (or a new one) fits.
3. **Views** (`harness-native/scene/10-harness.flecs`, `hn.*`): prefabs composed of primitives. Rust binds model to
   views by key with `harness_native_ui_view::for_each`/`for_each_of`/`lazy_for_each_of`.
4. **Tokens and theme** (`harness-native/scene/00-tokens.flecs`, `prefab hn.Theme` in 10-harness.flecs): every size,
   colour, radius and spring is an `export const` token; the user's Ghostty config (read by `harness-ghostty-config`,
   applied in `harness-native/src/app/ghostty.rs`) retints the theme-driven ones at runtime.
5. **Keys** (`harness-native/scene/20-keys.flecs`): bindings are data, see below.

harness-native is a library (`src/lib.rs`; the binary is a thin `main`). `harness_native::scene::world()` is layers 1 to 4
loaded headless, and every host stands on it: the app's window, its tests, and `harness-video`, which loads a video's
scene into it, so a video uses the same tokens, prims and `hn.*` widgets as the app (see `harness-video/README.md`).
A part both should show goes in a widget file or the prims, never in a video's scene.

Rust owns only: the terminal grid (thousands of cells a frame, never entities), key dispatch, the layout solver,
springs, drawing (shared with harness-video in `harness-native-render`: Metal, text, the paint pass), the daemon link. Everything a designer would tweak lives in script.

## The primitive vocabulary (SwiftUI's, as components)

| Want | Use |
| --- | --- |
| HStack / VStack / ZStack | `ui.Row`, `ui.Column`, `ui.ZStack` (`Flex: {axis, gap, align, justify, wrap}`) |
| Spacer, Divider | `ui.Spacer`, `ui.Divider` (`hn.Seam` for the 1pt vertical hairline) |
| .frame / .padding | `Size: {w, h}`, each axis `{kind: Fixed/Grow/Fit/Percent, value, min, max}`; `Padding: {l, r, t, b}`. Widths are proposals (SwiftUI's): a Fit node takes the less of its ideal width and the room, `max` is `.frame(maxWidth:)`, `FixedSize` is `.fixedSize()`; text wraps at what it gets (solve.rs's module doc) |
| Grid / LazyVGrid | `ui.Grid` + `ui.GridRow`/`ui.GridColumn`; `ui.AdaptiveGrid` with `GridColumns: {min, max}` |
| Long lists | `ui.LazyColumn`/`ui.LazyGrid` (`Lazy: {count, row, overscan}`) in a `ui.Scroll`: only visible rows exist (the Archive) |
| Words in a hugging plate (chat bubble, callout) | `ui.Bubble` (a Fit `ui.Box` capped by `Size.w.max`; its words wrap past it) |
| Text | `ui.Words` (wraps) / `ui.Line` / `ui.Lead` + `TextStyle`; `ui.LineMiddle`/`ui.LineHead` or `Truncate: {mode: Head/Middle/Tail}`; one line in rows, never wrap |
| Who truncates first | `ui.Lead` (`ShrinkFirst`), `LayoutPriority {value}`, `FixedSize {h, v}` (name truncates before badge) |
| Alignment | `Align: FirstBaseline/LastBaseline`, `AlignSelf`, `AlignmentGuide {dx, dy}` |
| Background / overlay | `Background` / `Overlay` tags on a child (laid to the parent's frame, out of flow); floating: `ui.Overlay` + `(AnchoredTo, node)` |
| Menu, popover, hover card past the window's edge | `Windowed` on the floating node (`ui.Menu`, `ui.Overlay`, `hn.GenFloat` carry it): its own borderless child window (`harness-native/src/window/overlay.rs`), placed and kept on the screen's visible frame (`Screen`), the system shadow under it; input stays in the main view's points. Toasts, sheets and dialogs over a pane stay in the window |
| Title-bar inset | `SafeAreaInset` / `SafeAreaPadding`, never hardcoded top padding |
| Responsive | `ui.ViewThatFits` (first child variant that fits) |
| Button / icon button | `ui.Button`; `ui.IconButton` (square wash hugging one `ui.Symbol`; never a full-width row wash) |
| Icons | `ui.Symbol` + `Icon: {glyph}` (den's icon set); `ui.Icon` + `Mark: {name}` for brand marks (vector, Slug) |
| Tooltip | `Tooltip: {text: "..."}` on any node (system tooltip; nearest ancestor's wins) |
| Pointer shape | `Cursor: {shape: Text}` on a node (nearest ancestor's wins; makes it a hit target): Text on what takes typing or selection, Arrow on controls, Hand only on links (macOS's conventions); the host's zones (terminal grid, sidebar edge) and drags in `harness-native/src/app/pointer.rs` |
| Scrolling | `ui.Scroll` (clips, shifts by `ScrollState`) carries `ui.Scroller`, macOS's overlay scroller: knob and track parts (`ScrollerPart`) layout places from `ScrollExtent`, motion fades and widens (`ScrollerState`), input drags and pages (`ScrollerGrip`); tokens `scroller_*`, `NoScroller` turns it off, `ScrollFed` lets a host feed the extent (the terminal), `scroll_quietly` moves without showing it. A host running its own physics (the tree's, harness-native-scroll-physics, in `harness-native/src/app/sidebar.rs`; a chat's) adopts a `ScrollState` the scroller wrote |
| Motion | `MatchedGeometry {id}` (selection pill), `Transition: {kind: Opacity/Slide/Scale/Move}`, `Animate`, `HoverAmt`/`HoverLift` springs; spring params are tokens |
| Pose / timeline | `Pose: {dx, dy, shrink, fade, cut}` (0 at rest; draw folds it into the subtree, layout ignores it); a `Cue` child animates any member by path over the clock (`harness-native-ui-timeline`; `cue.FadeIn`/`FadeOut`/`PushIn` in prims' cue.flecs name the moves). Only `harness-video` applies cues; the app's frame does not |
| Text motion, code colour | `GlyphsIn: {at, per, dur, by, ease}` (glyph-by-glyph arrival), `Caret: {at, until, blink, ink}`, `Sheen: {period, rest, band, strength, still, ease, bands, phase, scatter, dark, light}` (a band of light sweeping through the words, the sign of work: `ui.Shimmer`, a working row's title; strength 0 is off and free), `Highlight: {file}` (tree-sitter, inks from `code_ink.<capture>` tokens). Words always break at the width their box has this frame, so they re-wrap live through a fold, instantly (only broken again over glyphs shaped once, `Layout::wrap`). Never lay words out at a rest width: `LaysAtRest` is for a terminal's grid only |
| A running step's block | `ui.Block` + `PixelBlock: {face, turn}`: a pixel-art block (a game's item icon) whose face names what a step does (`exec`, `read`, `edit`, `net`, `think`, ...; drawn in harness-native-render block.rs, no file); set another face and it turns a quarter to it. The one coloured, moving thing in the app (a running step's line) |
| Pixel art | `Picture: {handle}` + `Pixelated`, `Frames: {count, fps}` for a sheet; `BlazeMark: {start, exit}` for the live fire wordmark (`harness-native-blaze`). Drawn as one texture, texels fetched not filtered: never a panel per cell (those seam when scaled) |
| Clip | `Clip` (the sidebar fold lays out at full width and clips; never relayout at in-between widths) |

New UI need not covered here: add a primitive (component + prim prefab + a gallery example under
`harness-native-ui-prims/scene/examples/`), then use it. Gallery: `HARNESS_NATIVE_GALLERY=1` with `HARNESS_NATIVE_SHOT`.

## Today: prefabs filled from Rust

- Views are prefabs; Rust keeps rows by key with `for_each`/`for_each_of` (`ui.List` or a column) and
  `lazy_for_each`/`lazy_for_each_of` (a `Lazy` container), and sets what differs through `View::part("name")`
  (all in `harness-native-ui-view/src/lib.rs`).
- Write new views so they convert mechanically to the 4.2 style below: data in, parts derived from it.

## Flecs 4.2 traps (the vendored version)

- A script has at most 64 reactive inputs: every distinct `$prop` and `$token` it reads and every `entity[Component]`
  read count, and past 64 the whole scene refuses to load (the app exits at launch). Keep fixed looks out of templates:
  a token-heavy value (an icon's size and padding) goes in a prefab in a prefab-only file (tree.flecs,
  parts.flecs) that the template's part inherits (`kind_icon : hn.tree.Kind {}`) or reads once
  (`Fill: hn.Kind.shell[Fill]`); what only Rust knows (a kind's mark) Rust sets. Two changes that each fit can
  together pass 64 on main: `scenes_load` is the check, run it on the merged tree.

- The 4.2 changes (explicit deferral, `Copy`, Parent storage) are in the `flecs` skill. Here: mutate from systems
  through the stage (`crates/harness/vendor/hyperion-ecs/src/system.rs`: `Iter::world` and the stage accessor) or collect, then write;
  never rely on implicit `defer_begin`.
- Script prefab/template children default to Parent storage in 4.2. The shutdown abort it caused
  (flecs_non_fragmenting_childof_move) is fixed in the fork (upstream flecs#2384), but the views still find children
  through ChildOf tables (without `@tree ChildOf` the window draws empty), so each top-level scene scope keeps
  `@tree ChildOf` until they read Parent children too.
- Since 4.2, prefabs, constraint traits, cached queries and query plans are addons: features of `hyperion-flecs-c`.
- flecs orders the systems of one phase by entity id, not by when they were made (a scene load recycles ids), so a
  system that must see another's results in the same `progress` goes in a later `Phase` (harness-native-flecs: the
  layout pass in `Layout`, hits and matched geometry in `AfterLayout`, the pointer's resolve in `Resolve`, the draw pass
  in `Draw`) and declares what it `reads`/`writes` so flecs merges first. Zoom is a uniform scale: layout, draw and input
  are in view points (window points over `App::zoom()`), plates snap to device pixels at rest, and matched geometry lands
  (never slides) when the room changes.

## Target: reactive templates (4.2 has them; no view uses them yet)

Views are being converted from the prefab style above; write new keyed views as templates once the conversion lands.
A row/pane/overlay is a `template` whose `prop`s are its data; conditional parts are `if` blocks. Fine-grained
reactivity re-runs only the statements that read a changed prop, and children survive, so springs, hover and
matched-geometry state on them keep running.

```
template hn.ObjRow {
  prop name = ""
  prop kind = Kind.Shell
  prop depth = 0
  prop code = 0
  prop folded = false
  prop has_children = false

  ui.Row { Padding: {l: depth * $indent} }
  if has_children { chevron : hn.Disclosure { Rotation: {deg: match folded { true: 0; false: 90 }} } }
  icon : ui.Icon { Mark: {name: kind_mark(kind)} }   // fn kind_mark in script
  label : ui.Lead { Text: {s: name} }
  if code != 0 { badge : hn.Badge { Text: {s: "{code}"} } }

  async {                                             // small behaviour lives with the widget
    while true { await on.activate(this); folded = !folded }   // flecs.script.input has no double-click
  }
}
```

- Rust sets the instance's props from the model (one `set` per changed row) and never pokes `part("...")`.
- Use template interfaces for families (an interface struct `Row`, `template SessionRow : Row`), and pass a
  template as a prop (`prop row: template Row`) to share list code between sidebar and archive.
- The language itself (props, muts, interfaces, `for`, `fn`, `include`, string formats, vectors, maps, `export mut`
  tokens Rust may change, async and input events) is the `flecs` skill's `script.md`.
- `async` + `flecs.script.input` must be bridged to our pointer/key system; if a behaviour needs the hot path
  (terminal input, key dispatch), keep it in Rust.

## Tokens, theme and look

- One theme: every colour is a role at the top of 00-tokens.flecs (`tone_<role>`; dark is near-black with a cool
  undertone and one blue for links and focus rings, light the blaze site's paper): grounds `ground`, `sidebar`, `card`, `raised`
  (what floats, solid), `well` (fields, code), `term_ground`; lines
  `line`, `line_strong`; inks `ink_strong`, `ink`, `ink_muted`, `ink_faint`; washes `hover`, `wash`, `selection`,
  `selection_focus`, `press`, `scrim`; `accent`, `accent_wash`, `on_accent`, `link`, `code_ink`; states `success`,
  `warning`, `error` and their `_wash`. Below them sit the palettes (kinds, brand marks, file types, chart series,
  GitHub states, `code_ink.*`), colours that name a thing. A widget names a role (`Fill: $tone_card`), never a colour,
  and never a new one-off token: add a role if none fits. `no_raw_colours` (harness-native's app.rs tests) fails on a
  colour literal elsewhere. A video (harness-video) is loaded on these same tokens; only what is its own is in a token
  file beside its scene.
- Sizes, radii and springs are `export const` tokens in 00-tokens.flecs too, never literals in a view.
- Colour, size and spring roles come from `hn.Theme` children (`hn.Theme.card` reads `$tone_card`: a token named like
  the entity that reads it does not resolve, flecs#2389); Rust reads them by name (`harness-native/src/look.rs` `role`,
  `tone`) and panics on a missing one.
- The sidebar is the main card's own ground with one `line` hairline (hn.Seam) between them: no lighter slab, no
  shadow. Only the terminal follows the user's Ghostty theme: its palette, cursor, font, padding and `term_ground` (a
  terminal pane's ground, src/app/ghostty.rs); the app's chrome, and code in a chat (`code_ink.*`, JetBrains' Islands
  Dark editor scheme), stay the theme's.
- Taste: dark, sleek and quiet, a cockpit at night, native macOS underneath. Concretely:
  - **Near-black, few steps.** Grounds a step or two apart on a cool near-black; hairlines barely over the ground; no
    grey slabs. Colour only where it means something (a state, the one accent, a series), never per kind for fun.
  - **Ink only.** Four inks on one ground. A kind's mark, a brand's mark, a slider, a highlight and a selection are ink
    or a step of grey, never a hue. The accent is for a link and a focus ring; red is for a real failure (an exit by
    SIGINT or SIGTERM is "stopped", not a failure).
  - **Plates only where something floats or is typed into.** One hairline, no shadow, no glass, corners of 8. A menu
    is ui.MenuPlate's solid plate in a window of its own.
  - **Type is the hierarchy.** One UI family (Inter) at macOS sizes; weight, size and ink tone separate levels before
    any box or rule does. Rows carry no noise: what matters now trails a row (what an agent is doing, the command or
    file), when is a tooltip, never a timestamp column.
  - **No sparkles.** Every glyph is monochrome and minimal, one weight (the codicons' line): never a sparkle, star
    burst, gradient or any other decorative "AI" mark, for an agent, a model, a suggestion or anything else. An agent is
    `cod-person` (`look::kind_icon`) or, on a chip, its state (`hn.chat_status`); a brand's own vector mark
    (`hn.Kind.<kind>`) is the only exception.
  - **Live work says what and how long.** Anything running shows its live step in words (the step, never the tool:
    "blaze check · 6 min", not "ts") and its elapsed time, ticking, with the one breathing glyph (the only motion left), so a long wait never
    looks hung; a card under the pointer says the rest (chat.rs `fill_chat_activity`).
  - **State by ink, not marks.** A working row's mark is in the strong ink and its step trails it in words; at rest
    the mark is faint; ended, the title is muted; failure is the error ink. No badges, no sheen, no dots.
  - **Show what changed, not what was looked at.** Summaries name their targets ("Edited chat.rs, ran cargo test");
    reads and listings are said only when nothing else happened.
  - **Lists are live.** A tree shows what is running or just finished; what is done leaves on its own for the Archive.
  - **Chrome only when asked.** A pane shows its content; its toolbar comes when it is used (the browser pane's
    address on cmd+L), a detail view when its summary is pressed (the context donut opening to its parts).
  - **Nothing moves on its own and nothing eases.** A hover, a menu's highlight, a menu, a popover and a sheet are
    there at once and gone at once (no `Transition`, no hover spring), so two states never show together. Ask before
    adding any motion. What still moves follows the hand or the content: a scroll, a fold, text streaming in.
  - **Nothing written while you were away animates.** Text arriving in a pane you are watching streams in; switching
    to it shows what came meanwhile at rest.

## Keybindings are data

`harness-native/scene/20-keys.flecs`: `act.*` action entities (params such as `SpawnKind`, `ZoomStep`, `SelectN`,
`Bytes` on `act.SendBytes` children; `MenuItem`, `Selector`), `key.* { Keybind: {key, cmd, shift, opt, ctrl};
(Does, act.X); (When, ctx.Tree) }`. No `When` holds everywhere; the innermost active context wins (`ctx.Palette` or `ctx.Picker`,
`ctx.Archive`, then `ctx.Tree` over `ctx.Terminal`), later bindings win, unbound keys go to the terminal. The menu bar,
palette hints and sidebar cmd+1-9 hints are generated from the same entities. User overrides:
`$HARNESS_HOME/keys.flecs` (default `~/.harness`), run after the scene, reloaded when it changes. A new binding to an
existing action is entities only; a new action also needs a handler under its name in `harness-native/src/app/keys.rs`.

## Verify (no focus-taking, ever)

- First, after any `.flecs` change: `blaze test -p harness-native scenes_load` (from `crates/harness/`; only blaze
  reads the workspace, since Kip's shaders are a blaze device build, `artifact = "gpu"`). It builds the world headless (no window, Metal or daemon), loads every scene script (tokens, prims,
  the harness scene and its widgets, keys, the prims gallery and every example) and fails with flecs's error and line. A
  scene that does not load (a prefab named like a component it sets, an entity named by a script keyword such as `new`,
  an unknown token) otherwise only shows at launch, as an empty window. Run it before every push.
- Offscreen: `HARNESS_NATIVE_SHOT=out.png HARNESS_NATIVE_SHOT_SECS=2` (default 2.5; + `HARNESS_NATIVE_SHOT_HOVER=x,y,t`
  in window points, `HARNESS_NATIVE_SHOT_KEY=k,t`, `HARNESS_NATIVE_SHOT_SEQ=t0,t1` for frame sequences,
  `HARNESS_NATIVE_SIDEBAR=0`, `HARNESS_NATIVE_APPEARANCE=light`); see `harness-native/src/shot.rs` for the rest.
- A real window without activation: `HARNESS_NATIVE_NO_ACTIVATE=1`. Never activate windows or synthesize input
  on the user's machine; use a private `HARNESS_HOME` daemon for tests.
- The system pointer isn't in shots: `HARNESS_NATIVE_CURSOR_LOG=1` logs each shape change, a shot's hovers (`x,y,t;x,y,t`) log
  theirs, F3's `ptr` line shows it; `HARNESS_NATIVE_GALLERY=prims` draws one of every primitive.
- F3 shows the stats overlay (fps, cpu/gpu frame times, redraw reason, entity/draw counts); with nothing moving its
  state line must read idle.
- Read the shot (or a 2x crop) before saying a visual change is done; measure alignment in pixels.
