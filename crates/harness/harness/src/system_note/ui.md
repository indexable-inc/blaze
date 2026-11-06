To show a result as UI instead of prose (a dashboard, a comparison, a summary), write a ts cell that is Svelte
5 markup (start it with <script> or an element) using Column, Row, Text (tone: ink muted strong error accent
success warning), Markdown, Code, Table (rows of records; query, onselect), Badge, Stat (label, value, note),
Card (title), Divider and Spacer; runes only ($state, $derived, $effect, $props), and the person sees it drawn
under the cell. For charts use <Chart kind="bar|stacked|line|area|scatter|pie|donut" data={[...]} or
series={[{name, data}]} labels={[...]} unit height /> (pie and donut take data as [{label, value}]); never
draw a chart by hand. For drawings, SVG (svg, g, path, rect, circle, ellipse, line, polyline, polygon, text)
or Canvas (view, Group, Path, Rect, Circle, Line, Polyline, Text); inks are tone names (ink, muted, accent,
success, warning, error, faint, series1..series8), never colours. For input use Button (label, value,
onclick), Toggle, Checkbox, Slider, NumberField, Input, TextArea, Select, Choice and Radio (options),
ColorPicker ("#rrggbb" or tone swatches; it sends #rrggbb), bound to $state with bind:value (Slider,
NumberField, Input, TextArea, Toggle), bind:checked (Checkbox) or bind:selected (Select, Choice, Radio,
ColorPicker); DropZone (ondrop gets the dropped files' paths); what the person does runs the cell's handlers
and redraws it. Layout: Grid (columns), Wrap, Scroll, Split (bind:ratio), Stack (align) with Layer (anchor);
folding and floating: Tabs (tabs, bind:selected; pick the body with {#if}), Disclosure (title) in an
Accordion, Popover, Menu (items; context opens it on a right press), Dialog and Sheet (bind:open on each) and
Toast; status, pictures and files: Progress, Loading, Skeleton, Icon (kind: a file kind), Image (src: a file
path), Chip, Tile (onclick), FileRef (path), File (a listing row: name, size, modified) and CellRef
(task="t3": another cell of yours, live); Kip, the app's mascot, in a mood (mood: wave happy celebrate
thinking working oops sleepy idle, a few in turn as "thinking then celebrate"; size 20 to 160) for a greeting
or an outcome, sparingly. Video plays a file or http(s) URL (muted, with controls; autoplay loop,
bind:currentTime, bind:paused): <Video id="clip" src="~/Movies/a.mp4" autoplay loop />. Shader runs a Slang
fragment shader every frame, `float4 main(ShaderInput in)` between its tags (in.uv, in.pixel, in.resolution,
in.time, in.pointer, in.hovered, in.u.<uniform>; texture(name, uv)); uniforms and textures take values or
'@id' of an element in the cell (a Slider's live value, a Video's frames): <Shader aspect="16:9" textures={{
clip: '@clip' }} uniforms={{ gain: '@gain' }}>float4 main(ShaderInput in) { return texture(clip, in.uv) *
in.u.gain; }</Shader>. Every element also takes width, height (and min/max), grow, padX, padY, a sprung pose
(x, y, scale, rotate, opacity), z, hover={{scale: 1.05}}, enter and exit (or Svelte's in:, out:, transition:
fly fade scale slide, and animate:flip), and onpointerdown/move/up, ondrag, onwheel, onhover, onkeydown,
onfocus, onblur. To ask the person and wait for the answer, await ask(text, {options, default}). An element or
prop not listed is an error naming it and its line.
