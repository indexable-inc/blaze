---
name: ui
description: Cells as Svelte 5 UI: every element and prop the chat draws (text, tables, stats, charts, drawings, inputs, layout, dialogs, media, shaders, motion) and the rules; read before writing a UI cell.
---
To show a result as UI instead of prose (a dashboard, a comparison, a summary), write a ts cell that is Svelte 5
markup (start it with <script> or an element); runes only ($state, $derived, $effect, $props), and the person sees it
drawn under the cell. An element or prop not listed here is an error naming it and its line.

Make it something to use, not just look at: a button that runs the next step, a toggle or slider that filters
the table, a diagram that steps through what happened (like ix.dev's learn pages: one idea per view, a small
animated diagram, one clear action). Keep each view to one idea; ask with `ask(...)` when the person's choice
decides what you do next.

A report is progressive: the verdict on one line, each part (what changed, risks, what to verify) in a
`Disclosure` under it, closed:

```svelte
<Column>
  <Text tone="strong">Fable 5.2 is in the picker; malloc routing is committed, not yet built.</Text>
  <Accordion>
    <Disclosure title="What changed"><Text>...</Text></Disclosure>
    <Disclosure title="Risks"><Text tone="warning">...</Text></Disclosure>
    <Disclosure title="What to verify"><Text>...</Text></Disclosure>
  </Accordion>
</Column>
```

An explanation is a comic: two to four panels side by side, each a `Card` holding one small drawing (SVG, about 160 by
110, a few simple shapes and lines in tone inks: people as a circle on a line, a thing as its outline, an arrow for
what changes) and one caption line under it. One moment a panel, in order, so the row reads as a story; the last
panel lands the point. Draw the idea, never decorate; leave air; no more than a dozen shapes a panel. `Kip` may be the
character (he has moods).

```svelte
<Grid columns={3}>
  <Card>
    <svg viewBox="0 0 160 110">
      <circle cx="50" cy="40" r="10" stroke="ink" fill="none" /><line x1="50" y1="50" x2="50" y2="82" stroke="ink" />
      <circle cx="112" cy="46" r="10" stroke="muted" fill="none" /><line x1="112" y1="56" x2="112" y2="86" stroke="muted" />
    </svg>
    <Text tone="muted">Two people, each with their own day.</Text>
  </Card>
  <Card>...the next moment...</Card>
  <Card>...the point...</Card>
</Grid>
```

Content: Column, Row, Text (tone: ink muted strong error accent success warning), Markdown, Code, Table (rows of
records; query, onselect; a cell that is an inline reference, `from: '<Path loc="/abs/file.md" line={12}>rule 48</Path>'`,
draws as its chip: give a row its source that way), Badge, Stat (label, value, note), Card (title), Divider and Spacer.

Charts: <Chart kind="bar|stacked|line|area|scatter|pie|donut" data={[...]} or series={[{name, data}]} labels={[...]}
unit height /> (pie and donut take data as [{label, value}]); never draw a chart by hand.

Drawings: SVG (svg, g, path, rect, circle, ellipse, line, polyline, polygon, text) or Canvas (view, Group, Path, Rect,
Circle, Line, Polyline, Text); inks are tone names (ink, muted, accent, success, warning, error, faint,
series1..series8), never colours.

Input: Button (label, value, onclick), Toggle, Checkbox, Slider, NumberField, Input, TextArea, Select, Choice and Radio
(options), ColorPicker ("#rrggbb" or tone swatches; it sends #rrggbb), bound to $state with bind:value (Slider,
NumberField, Input, TextArea, Toggle), bind:checked (Checkbox) or bind:selected (Select, Choice, Radio, ColorPicker);
DropZone (ondrop gets the dropped files' paths); what the person does runs the cell's handlers and redraws it.

Layout: Grid (columns), Wrap, Scroll, Split (bind:ratio), Stack (align) with Layer (anchor). Folding and floating:
Tabs (tabs, bind:selected; pick the body with {#if}), Disclosure (title) in an Accordion, Popover, Menu (items;
context opens it on a right press), Dialog and Sheet (bind:open on each) and Toast.

Status, pictures and files: Progress, Loading, Skeleton, Icon (kind: a file kind), Image (src: a file path), Chip, Tile
(onclick), FileRef (path), File (a listing row: name, size, modified) and CellRef (task="t3": another cell of yours,
live); Kip, the app's mascot, in a mood (mood: wave happy celebrate thinking working oops sleepy idle, a few in turn as
"thinking then celebrate"; size 20 to 160) for a greeting or an outcome, sparingly.

Media: Video plays a file or http(s) URL (muted, with controls; autoplay loop, bind:currentTime, bind:paused): <Video
id="clip" src="~/Movies/a.mp4" autoplay loop />. Shader runs a Slang fragment shader every frame, `float4
main(ShaderInput in)` between its tags (in.uv, in.pixel, in.resolution, in.time, in.pointer, in.hovered,
in.u.<uniform>; texture(name, uv)); uniforms and textures take values or '@id' of an element in the cell (a Slider's
live value, a Video's frames): <Shader aspect="16:9" textures={{ clip: '@clip' }} uniforms={{ gain: '@gain' }}>float4
main(ShaderInput in) { return texture(clip, in.uv) * in.u.gain; }</Shader>.

Every element also takes width, height (and min/max), grow, padX, padY, a sprung pose (x, y, scale, rotate, opacity),
z, hover={{scale: 1.05}}, enter and exit (or Svelte's in:, out:, transition: fly fade scale slide, and animate:flip),
and onpointerdown/move/up, ondrag, onwheel, onhover, onkeydown, onfocus, onblur.

To ask the person and wait for the answer, await ask(text, {options, default}).
