---
name: flecs-ui
description: "Building native UI as Flecs (the harness app in crates/harness/): views are flecs script prefabs over den's primitives (ui.Row/Column/Grid/LazyColumn/IconButton...), state is components and relationships, Rust only feeds data in and runs the hot paths. Covers the layering, the primitive vocabulary, tokens and themes, keybindings as data, how to verify with offscreen shots, the Flecs 4.2 traps, and the 4.2 reactive-template style views are moving to. Load before touching any crates/harness/**/*.flecs, the crates/harness/harness-native-ui-* crates, or adding a widget, row, pane or shortcut."
---

# Native UI as Flecs

General Flecs rules, traits and script syntax are in the `flecs` skill; this one is how the harness app builds UI
with them. Paths below are relative to `crates/harness/`. The app is copied from den's native stack: flecs v4.2.0
(vendored in `vendor/flecs` from the indexable-inc/flecs fork, branch `indexable/v4.2`; see `vendor/flecs/SOURCE.md`), Metal through the
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

Rust owns only: the terminal grid (thousands of cells a frame, never entities), key dispatch, the layout solver,
springs, drawing, the daemon link. Everything a designer would tweak lives in script.

## The primitive vocabulary (SwiftUI's, as components)

| Want | Use |
| --- | --- |
| HStack / VStack / ZStack | `ui.Row`, `ui.Column`, `ui.ZStack` (`Flex: {axis, gap, align, justify, wrap}`) |
| Spacer, Divider | `ui.Spacer`, `ui.Divider` (`hn.Seam` for the 1pt vertical hairline) |
| .frame / .padding | `Size: {w, h}`, each axis `{kind: Fixed/Grow/Fit/Percent, value, min, max}`; `Padding: {l, r, t, b}` |
| Grid / LazyVGrid | `ui.Grid` + `ui.GridRow`/`ui.GridColumn`; `ui.AdaptiveGrid` with `GridColumns: {min, max}` |
| Long lists | `ui.LazyColumn`/`ui.LazyGrid` (`Lazy: {count, row, overscan}`) in a `ui.Scroll`: only visible rows exist (the Archive) |
| Text | `ui.Words` (wraps) / `ui.Line` / `ui.Lead` + `TextStyle`; `ui.LineMiddle`/`ui.LineHead` or `Truncate: {mode: Head/Middle/Tail}`; one line in rows, never wrap |
| Who truncates first | `ui.Lead` (`ShrinkFirst`), `LayoutPriority {value}`, `FixedSize {h, v}` (name truncates before badge) |
| Alignment | `Align: FirstBaseline/LastBaseline`, `AlignSelf`, `AlignmentGuide {dx, dy}` |
| Background / overlay | `Background` / `Overlay` tags on a child (laid to the parent's frame, out of flow); floating: `ui.Overlay` + `(AnchoredTo, node)` |
| Title-bar inset | `SafeAreaInset` / `SafeAreaPadding`, never hardcoded top padding |
| Responsive | `ui.ViewThatFits` (first child variant that fits) |
| Button / icon button | `ui.Button`; `ui.IconButton` (square wash hugging one `ui.Symbol`; never a full-width row wash) |
| Icons | `ui.Symbol` + `Icon: {glyph}` (den's icon set); `ui.Icon` + `Mark: {name}` for brand marks (vector, Slug) |
| Tooltip | `Tooltip: {text: "..."}` on any node (system tooltip; nearest ancestor's wins) |
| Scrolling | `ui.Scroll` (clips, shifts by `ScrollState`); the tree's inertia (harness-native-scroll-physics), knob and edge fades are in `harness-native/src/app/sidebar.rs` |
| Motion | `MatchedGeometry {id}` (selection pill), `Transition: {kind: Opacity/Slide/Scale/Move}`, `Animate`, `HoverAmt`/`HoverLift` springs; spring params are tokens |
| Clip | `Clip` (the sidebar fold lays out at full width and clips; never relayout at in-between widths) |

New UI need not covered here: add a primitive (component + prim prefab + a gallery example under
`harness-native-ui-prims/scene/examples/`), then use it. Gallery: `HARNESS_NATIVE_GALLERY=1` with `HARNESS_NATIVE_SHOT`.

## Today: prefabs filled from Rust

- Views are prefabs; Rust keeps rows by key with `for_each`/`for_each_of` (`ui.List` or a column) and
  `lazy_for_each`/`lazy_for_each_of` (a `Lazy` container), and sets what differs through `View::part("name")`
  (all in `harness-native-ui-view/src/lib.rs`).
- Write new views so they convert mechanically to the 4.2 style below: data in, parts derived from it.

## Flecs 4.2 traps (the vendored version)

- The 4.2 changes (explicit deferral, `Copy`, Parent storage) are in the `flecs` skill. Here: mutate from systems
  through the stage (`vendor/hyperion-ecs/src/system.rs`: `Iter::world` and the stage accessor) or collect, then write;
  never rely on implicit `defer_begin`.
- Script prefab/template children default to Parent storage in 4.2. The shutdown abort it caused
  (flecs_non_fragmenting_childof_move) is fixed in the fork (upstream flecs#2384), but the views still find children
  through ChildOf tables (without `@tree ChildOf` the window draws empty), so each top-level scene scope keeps
  `@tree ChildOf` until they read Parent children too.
- Since 4.2, prefabs, constraint traits, cached queries and query plans are addons: features of `hyperion-flecs-c`.

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

- No literal colour, size, radius or spring in a view: add an `export const` token in 00-tokens.flecs.
- Colour, size and spring roles come from `hn.Theme` children; Rust reads them by name (`harness-native/src/look.rs`
  `role`) and panics on a missing one.
- The terminal ground, card, sidebar tint (`tone_side_tint`, same ground as the terminal, one `tone_seam` hairline,
  no shadow) and title inks follow the user's Ghostty theme (dark/light) live; terminal palette, cursor, font and
  padding come from it too.
- Taste: native macOS, minimal. One font family for UI (Inter) at macOS sizes, hierarchy by weight/size/tone,
  no noise on rows, meta on hover, highlights hug their target, motion on springs, never a step.

## Keybindings are data

`harness-native/scene/20-keys.flecs`: `act.*` action entities (params such as `SpawnKind`, `ZoomStep`, `SelectN`,
`Bytes` on `act.SendBytes` children; `MenuItem`, `Selector`), `key.* { Keybind: {key, cmd, shift, opt, ctrl};
(Does, act.X); (When, ctx.Tree) }`. No `When` holds everywhere; the innermost active context wins (`ctx.Palette`,
`ctx.Archive`, then `ctx.Tree` over `ctx.Terminal`), later bindings win, unbound keys go to the terminal. The menu bar,
palette hints and sidebar cmd+1-9 hints are generated from the same entities. User overrides:
`$HARNESS_HOME/keys.flecs` (default `~/.harness`), run after the scene, reloaded when it changes. A new binding to an
existing action is entities only; a new action also needs a handler under its name in `harness-native/src/app/keys.rs`.

## Verify (no focus-taking, ever)

- Offscreen: `HARNESS_NATIVE_SHOT=out.png HARNESS_NATIVE_SHOT_SECS=2` (default 2.5; + `HARNESS_NATIVE_SHOT_HOVER=x,y,t`
  in window points, `HARNESS_NATIVE_SHOT_KEY=k,t`, `HARNESS_NATIVE_SHOT_SEQ=t0,t1` for frame sequences,
  `HARNESS_NATIVE_SIDEBAR=0`, `HARNESS_NATIVE_APPEARANCE=light`); see `harness-native/src/shot.rs` for the rest.
- A real window without activation: `HARNESS_NATIVE_NO_ACTIVATE=1`. Never activate windows or synthesize input
  on the user's machine; use a private `HARNESS_HOME` daemon for tests.
- F3 shows the stats overlay (fps, cpu/gpu frame times, redraw reason, entity/draw counts); with nothing moving its
  state line must read idle.
- Read the shot (or a 2x crop) before saying a visual change is done; measure alignment in pixels.
