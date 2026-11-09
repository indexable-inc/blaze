import type { ElementName } from './element.ts';

/*
 * The props of every element a cell can write as Svelte markup, as types. Events are lowercase, `onselect`, as Svelte 5 writes
 * them, and go to the harness app as `onSelect`. Cells are not type-checked (their types are erased), so these are the reference
 * an editor, a test or a component library written in TypeScript checks against; element.ts checks the same rules at run time
 * (PROPS and its rules), and the app draws them (harness-native/src/app/chat/genui.rs). Cut down from den's repl/src/ui/types.ts
 * to the elements the harness app draws, with den's props every element takes (`CommonProps`).
 */

/**
 * The ink of words, of a badge's plate and of a chart's bars or line: the one way an element takes a colour (there are no colour
 * names, hex values or styles). Any other value is an error at the cell's line.
 */
export type Tone = 'ink' | 'muted' | 'strong' | 'error' | 'accent' | 'success' | 'warning';

/** The ink of a shape's fill or stroke: a tone, the quiet `faint`, a chart's series inks, or `none`. */
export type Ink = Tone | 'faint' | 'series1' | 'series2' | 'series3' | 'series4' | 'series5' | 'series6' | 'series7' | 'series8' | 'none';

/** What a handler is called with: the element's value. */
export type Handle<Value = unknown> = (value: Value) => unknown;

/** How an element comes (`enter`) or goes (`exit`), sprung by the app; `transition:fly={{ y: 8 }}` writes it for you. */
export interface Transition {
  kind: 'fly' | 'fade' | 'scale' | 'slide';
  /** fly: where it comes from (points); fly, fade and scale: the opacity it starts at (0); scale: the size it grows from (0); slide: the axis it grows along (y). */
  x?: number; y?: number; opacity?: number; start?: number; axis?: 'x' | 'y';
  /** How long its spring takes to settle (400), and how long it waits first (0), in milliseconds. */
  ms?: number; delay?: number;
}
/** Where the pointer is on an element, in its points from its top left, and as a share of its size. */
export interface PointerAt { x: number; y: number; u: number; v: number }
/** A drag's step: how far since the last (`dx`, `dy`), where it is (`x`, `y`) and how far from where it began (`tx`, `ty`). */
export interface DragStep { dx: number; dy: number; x: number; y: number; tx: number; ty: number; phase: 'move' | 'end' }

/** What every element, a drawing's shapes and Groups too, takes besides its own: pose, motion, the pointer and the keys. */
export interface DrawCommonProps {
  /** Its pose, sprung whenever it changes: moved (points), scaled about its middle (0 to 8), turned (degrees; only a drawing's shapes and Groups turn), faded (0 to 1). */
  x?: number; y?: number; scale?: number; rotate?: number; opacity?: number;
  /** What is over what (higher is over). */
  z?: number;
  /** Eases it to where it moved, as `animate:flip` (which writes it) does. */
  flip?: boolean;
  /** How it comes and goes; `exit` is kept until it has played. */
  enter?: Transition; exit?: Transition;
  /** How it looks under the pointer, eased to and back. */
  hover?: { x?: number; y?: number; scale?: number; rotate?: number; opacity?: number; fill?: Ink; stroke?: Ink; tone?: Tone };
  /** Words shown when the pointer rests on it. */
  tip?: string;
  /** Any of these makes it a hit target; pointermove and drag come at most once a frame. */
  onpointerdown?: Handle<PointerAt>; onpointermove?: Handle<PointerAt>; onpointerup?: Handle<PointerAt>;
  ondrag?: Handle<DragStep>;
  onwheel?: Handle<{ dx: number; dy: number }>;
  /** The pointer came onto it (true) or left it (false). */
  onhover?: Handle<boolean>;
  /** A key while it has the keys (a press or Tab gives them to it): `'a'`, `'Enter'`, `'ArrowUp'`, `'Escape'`, ... */
  onkeydown?: Handle<{ key: string }>;
  onfocus?: Handle<null>; onblur?: Handle<null>;
}
/**
 * The props every element takes besides its own. An element that reads `width` or `height` as its own (Image, Chart, Scroll, Canvas)
 * keeps its own meaning, and so do a Rect's and a Canvas Text's x and y (geometry) and a Layer's (its offset from its anchor).
 */
export interface CommonProps extends DrawCommonProps {
  /** Points, fixed or bounded; what the element would be when not given. */
  width?: number; height?: number; minWidth?: number; maxWidth?: number; minHeight?: number; maxHeight?: number;
  /** Takes the room left in its Row or Column. */
  grow?: boolean;
  /** Padding inside it, in points, on top of its own. */
  padX?: number; padY?: number;
}

export interface ColumnProps extends CommonProps { gap?: number; pad?: number }
export interface RowProps extends CommonProps { gap?: number; pad?: number }
/**
 * Words. Inside a Canvas (or an <svg>) a Text is placed at a point instead: `x`, `y`, hung from it by `anchor`, `size` points tall,
 * in the `fill` ink (ink when not said); `tone` is then an error, and outside a drawing the drawing props are.
 */
export interface TextProps extends CommonProps {
  text?: string; tone?: Tone; bold?: boolean;
  x?: number; y?: number; anchor?: 'start' | 'middle' | 'end'; size?: number; fill?: Ink;
  /** Inside a drawing: a press sends `value`. */
  value?: unknown; onclick?: Handle;
}
export interface MarkdownProps extends CommonProps { text?: string }
export interface CodeProps extends CommonProps { text?: string; lang?: string }
export interface TableProps extends CommonProps {
  columns?: Array<string | { title: string; key?: string }>; rows?: unknown[];
  /** Keeps the rows matching these words, the match marked; "No rows match" when none do. */
  query?: string;
  /** A row pressed: its index in `rows`. */
  onselect?: Handle<number>;
  /** A cell that opens something pressed: the path it opens. */
  onopen?: Handle<string>;
}
export interface BadgeProps extends CommonProps { text?: string; tone?: Tone }
/** A number to read at a glance: a small `label`, a big `value` and a small `note` under it. */
export interface StatProps extends CommonProps { label?: string; value?: string | number; note?: string; tone?: Tone }
/** A plate that holds its children, under an optional `title`. */
export interface CardProps extends CommonProps { title?: string; gap?: number; pad?: number }
export interface DividerProps extends CommonProps {}
export interface SpacerProps extends CommonProps { size?: number }

/** One slice of a pie or a donut: a datum called `other...` is drawn last, in the quiet ink, however big. */
export interface ChartSlice { label: string; value: number }
/** One of several lines, areas or stacks: `[{name: 'read', data: [...]}, {name: 'write', data: [...]}]`. At most 8. */
export interface ChartSeries { name?: string; data: Array<number | [number, number]>; tone?: Tone }
export interface ChartProps extends CommonProps {
  /** Bars (the default), a line, a pie or a donut (the largest slice first), an area, a scatter of `[x, y]` points, or stacked bars of `series`. */
  kind?: 'bar' | 'line' | 'pie' | 'donut' | 'area' | 'scatter' | 'stacked';
  /** Numbers (bars, a line, an area), `[x, y]` points (a scatter) or `{label, value}` slices (a pie, a donut). At most 2000. */
  data?: Array<number | [number, number] | ChartSlice>;
  labels?: string[];
  series?: ChartSeries[];
  height?: number;
  /** What a value counts, said after it in the legend and the tip (`'GB'`; `'%'` says no share again). */
  unit?: string;
  /** A donut's middle: big words instead of its total, and a `caption` under them. */
  center?: string;
  caption?: string;
  /** A donut's hole, as a share of its width; how far the slice under the pointer comes out; the gap between slices (points). */
  hole?: number;
  pop?: number;
  sep?: number;
  /** How opaque an area's fill is (0 to 1). */
  fillOpacity?: number;
  /** The ink of its bars or its line; the accent when not given. */
  tone?: Tone;
  /** Whether the names of its slices or series are listed beside it; the app decides when not given. */
  legend?: boolean;
  /** A slice pressed: `{index, label, value}`. */
  onselect?: Handle<{ index: number; label: string; value: number }>;
}

/** An affine transform as SVG's `matrix(a b c d e f)`, or SVG transform words (`translate(10 0) rotate(45)`) that become one. */
export type Transform = string | [number, number, number, number, number, number];

/** Where a Canvas's shapes are drawn from: four numbers, `"minX minY width height"`, fitted in its box as SVG's viewBox is. */
export interface CanvasProps extends CommonProps { view: string | [number, number, number, number]; width?: number; height?: number; value?: unknown; onclick?: Handle }
/**
 * What every shape takes: its inks (a fill of ink when not said, unless it is an open line; no stroke when not said), how wide its
 * stroke is and how opaque each, and a press, which sends its `value` (null when it has none).
 */
export interface ShapeProps extends DrawCommonProps { fill?: Ink; stroke?: Ink; strokeWidth?: number; fillOpacity?: number; strokeOpacity?: number; transform?: Transform; value?: unknown; onclick?: Handle }
/** Shapes moved, scaled and turned together by `transform`, faded together by `opacity`; a press sends its `value`. */
export interface GroupProps extends DrawCommonProps { transform?: Transform; opacity?: number; value?: unknown; onclick?: Handle }
export interface PathProps extends ShapeProps { d: string; fillRule?: 'nonzero' | 'evenodd' }
export interface RectProps extends ShapeProps { x?: number; y?: number; width: number; height: number; radius?: number }
export interface CircleProps extends ShapeProps { cx: number; cy: number; r?: number; rx?: number; ry?: number }
export interface LineProps extends ShapeProps { x1: number; y1: number; x2: number; y2: number }
export interface PolylineProps extends ShapeProps { points: string | number[] | Array<[number, number]>; closed?: boolean }

/**
 * The controls. Each sends what the person did back to its handler (index.ts `dispatch`): a press is `onclick` (called with null), a
 * change `onchange` (with the new value), Enter in an Input or Cmd+Enter in a TextArea `onsubmit` (with the text). The bindable one
 * of each (`bind:value`, `bind:checked`, `bind:selected`) is written back first, then the handler runs. A control with neither a
 * binding nor a handler only shows its value. On the wire (element.ts `elementWire`) a handler is its id as `onClick`, `onChange`,
 * `onSubmit`, and defaults are filled in.
 */
/** A press, which sends `value` (null when not given). Its label is `label`, or the words between its tags (`<Button>Save</Button>`); its plate is plain unless `tone` says. */
export interface ButtonProps extends CommonProps { label?: string; tone?: 'accent' | 'error' | 'success' | 'warning' | 'muted'; value?: unknown; disabled?: boolean; onclick?: Handle }
/** A switch, on or off. Binds `value`. */
export interface ToggleProps extends CommonProps { id?: string; value?: boolean; label?: string; disabled?: boolean; onchange?: Handle<boolean> }
/** A box ticked or not. Binds `checked`. */
export interface CheckboxProps extends CommonProps { id?: string; checked?: boolean; label?: string; disabled?: boolean; onchange?: Handle<boolean> }
/** A number dragged from `min` (0) to `max` (100), snapped to `step`. Binds `value` (`min` when not said). */
export interface SliderProps extends CommonProps { id?: string; value?: number; min?: number; max?: number; step?: number; label?: string; disabled?: boolean; onchange?: Handle<number> }
/** A number typed or stepped by `step` (1) within `min` and `max`. Binds `value` (`min`, else 0, when not said). */
export interface NumberFieldProps extends CommonProps { id?: string; value?: number; min?: number; max?: number; step?: number; label?: string; disabled?: boolean; onchange?: Handle<number> }
/** One line of words; `onchange` hears them while typing (at most every 150 ms) and on leaving, `onsubmit` on Enter. Binds `value`. */
export interface InputProps extends CommonProps { value?: string; placeholder?: string; label?: string; disabled?: boolean; onchange?: Handle<string>; onsubmit?: Handle<string> }
/** Words over `lines` (3) lines; `onsubmit` on Cmd+Enter. Binds `value`. */
export interface TextAreaProps extends CommonProps { value?: string; placeholder?: string; lines?: number; disabled?: boolean; onchange?: Handle<string>; onsubmit?: Handle<string> }
/** An option: words (its label and its value), or a label with any JSON value. At most 200 options. */
export type ControlOption = string | number | { label: string; value?: unknown };
/** A field that drops its options to pick one. Binds `selected` (the picked option's value, null when none). */
export interface SelectProps extends CommonProps { options: ControlOption[]; selected?: unknown; placeholder?: string; disabled?: boolean; onchange?: Handle }
/** Options side by side as segments, one picked. Binds `selected`. */
export interface ChoiceProps extends CommonProps { options: ControlOption[]; selected?: unknown; disabled?: boolean; onchange?: Handle }
/** Options each with its round mark, one picked, in a column (or a `row`). Binds `selected`. */
export interface RadioProps extends CommonProps { options: ControlOption[]; selected?: unknown; row?: boolean; disabled?: boolean; onchange?: Handle }
/** A colour: `#rrggbb`, or a tone (its ink in the app). */
export type Colour = `#${string}` | Tone;
/**
 * A row of round swatches, the picked one ringed, and a last well that opens the system colour panel for any colour. Its swatches are
 * `options` (at most 24; the tones when not given). Binds `selected`; `onchange` gets the colour picked as `#rrggbb`.
 */
export interface ColorPickerProps extends CommonProps { options?: Colour[]; selected?: Colour; label?: string; disabled?: boolean; onchange?: Handle<string> }
/** A plate that lights while Finder files are carried over it; dropping them sends `ondrop` their absolute paths. */
export interface DropZoneProps extends CommonProps { label?: string; ondrop?: Handle<string[]> }

/*
 * Layout, folding, floating, status and pictures. Each goes to the app as exactly what element.ts `elementWire` gives it, defaults
 * filled in; the bindable ones (`bind:ratio`, `bind:selected`, `bind:open`) are written back from their event first, then the handler runs.
 */
/** Equal columns, row-major: `columns` of them (2 when neither is said), or as many as `fit` (each at least that many points wide). Gap 12. */
export interface GridProps extends CommonProps { columns?: number; fit?: number; gap?: number; pad?: number }
/** Where a Layer is pinned in its Stack, and where a Stack puts the children that are not Layers. */
export type LayerAnchor = 'top-left' | 'top' | 'top-right' | 'left' | 'center' | 'right' | 'bottom-left' | 'bottom' | 'bottom-right';
/** Children on top of one another in one box, as big as its largest child (Layers included), each that is not a Layer where `align` puts it; later ones are above. */
export interface StackProps extends CommonProps { align?: LayerAnchor }
/** A child of a Stack (only there) pinned at `anchor` (top-left), moved by `x`, `y` points; a higher `z` is above. Its content counts towards the Stack's size (`x` and `y` do not), and its content is laid in the Stack's whole frame. A press sends its `value`. */
export interface LayerProps extends CommonProps { anchor?: LayerAnchor; x?: number; y?: number; z?: number; value?: unknown; onclick?: Handle }
/** Children left to right, wrapping to the next line; gap 8. */
export interface WrapProps extends CommonProps { gap?: number }
/** Its children in a window `height` (240, at most 600) points tall, or wide when `horizontal`, that scrolls with the wheel. */
export interface ScrollProps extends CommonProps { height?: number; horizontal?: boolean }
/** Exactly two panes, side by side (or `vertical`ly), the first `ratio` (0.05 to 0.95, 0.5) of the room; dragging the divider sends the new ratio. Binds `ratio`. */
export interface SplitProps extends CommonProps { ratio?: number; vertical?: boolean; onchange?: Handle<number> }
/** A row of 1 to 20 tabs over its children; the cell shows the selected one's body with {#if}. A press sends the tab's value. Binds `selected` (the first tab's value when not said). */
export interface TabsProps extends CommonProps { tabs: ControlOption[]; selected?: unknown; onchange?: Handle }
/** Disclosures only, at most one open: opening one closes the open one (each hears `ontoggle`). */
export interface AccordionProps extends CommonProps {}
/** A head of `title` that shows its children when open; a press on the head sends `!open`. Binds `open`. */
export interface DisclosureProps extends CommonProps { title: string; open?: boolean; ontoggle?: Handle<boolean> }
/**
 * Its first child is the trigger, drawn in place; the rest float on `placement` side (bottom), lined up with its `align` (start) or
 * centre, while open. A press on the trigger toggles it (or the pointer over it, with `trigger="hover"`); a press outside or Escape
 * sends false. Binds `open`.
 */
export interface PopoverProps extends CommonProps { placement?: 'top' | 'bottom' | 'left' | 'right'; align?: 'start' | 'center'; trigger?: 'click' | 'hover'; open?: boolean; ontoggle?: Handle<boolean> }
/** A row of a Menu: words (its label and its value), '-' (a separator), or a row with a value, a red `danger`, `disabled`, and the `keys` it shows. */
export type MenuItem = string | number | '-' | { separator: true } | { label: string; value?: unknown; danger?: boolean; disabled?: boolean; keys?: string };
/** 1 to 50 rows under its one child, the trigger (or at the pointer on a right press of it, with `context`). Picking a row sends `onselect` with its value and closes it. Binds `open`. */
export interface MenuProps extends CommonProps { items: MenuItem[]; context?: boolean; open?: boolean; onselect?: Handle; ontoggle?: Handle<boolean> }
/** A modal over a scrim covering the pane. When `dismissable` (true), Escape or a press on the scrim sends `onclose`, and `bind:open` becomes false. A Sheet rises from the pane's foot. */
export interface DialogProps extends CommonProps { title?: string; open?: boolean; dismissable?: boolean; onclose?: Handle<false> }
export interface SheetProps extends CommonProps { title?: string; open?: boolean; dismissable?: boolean; onclose?: Handle<false> }
/** Words floating at the pane's foot for `duration` ms (4000; 0 stays), gone when pressed (`ondismiss`); shown again whenever its words change. Its text is also its children. */
export interface ToastProps extends CommonProps { text?: string; tone?: Tone; duration?: number; ondismiss?: Handle<null> }
/** A bar `value` (0) of `max` (1) full, with an optional `label`. */
export interface ProgressProps extends CommonProps { value?: number; max?: number; label?: string }
/** A spinner and quiet words (`Loading`); its label is also its children. */
export interface LoadingProps extends CommonProps { label?: string }
/** `lines` (3, 1 to 20) grey bars that breathe, where text is on its way. */
export interface SkeletonProps extends CommonProps { lines?: number }
/** A file kind: its glyph and colour in the app's file palette (harness-native media.rs `KINDS`), or a short way of saying one. */
export type FileKind =
  | 'rust' | 'typescript' | 'tsx' | 'javascript' | 'json' | 'toml' | 'yaml' | 'markdown' | 'nix' | 'shell' | 'python' | 'go'
  | 'html' | 'css' | 'image' | 'lock' | 'git' | 'folder' | 'folder_open' | 'text'
  | 'rs' | 'ts' | 'js' | 'jsx' | 'mjs' | 'md' | 'yml' | 'sh' | 'bash' | 'zsh' | 'py' | 'package' | 'picture' | 'photo' | 'png' | 'jpg'
  | 'svg' | 'dir' | 'directory' | 'folder-open' | 'file' | 'generic' | 'doc' | 'txt';
/** A file kind's glyph (`kind`), or a symbol or file kind by `name`, from the app's file palette and symbol set; one of them is needed. */
export interface IconProps extends CommonProps { name?: string; kind?: FileKind }
/** A file on disk (a path, relative ones from the working directory, `~/` the home directory, or a `{path}`), as the app is given it: an absolute path. */
export type FileSource = string | URL | Blob | { path: string };
/** A picture from a file (no http or data: URLs). Natural size, at most the column wide and 360 tall, unless `width`/`height` say; `fit` (contain) in a box of both. */
export interface ImageProps extends CommonProps { id?: string; src: FileSource; width?: number; height?: number; fit?: 'contain' | 'cover'; alt?: string }
/** A pill, brighter when `selected`. A press sends `value`, or `!selected` when it has none. Its label is also its children. */
export interface ChipProps extends CommonProps { label?: string; selected?: boolean; value?: unknown; onclick?: Handle }
/** Its children in a column, pressed as a whole (it lights under the pointer); a press sends `value`. Gap 8, pad 12. */
export interface TileProps extends CommonProps { gap?: number; pad?: number; value?: unknown; onclick?: Handle }
/** A file chip, its kind's icon and its name; a press opens the file (at `line`) as the transcript's file chips do (Option reveals it in Finder), then calls `onclick` with the path. */
export interface FileRefProps extends CommonProps { path: FileSource; line?: number; name?: string; kind?: FileKind; onclick?: Handle<string> }
/**
 * A row of a listing: its kind's icon, `name` (a trailing / is a folder), its `size` in bytes and when it was `modified` (words, a
 * Date, or milliseconds since the epoch as `ls()` gives). A press sends `value`, else the name. Given a `path`, it is that file's
 * FileRef chip instead (and takes what a FileRef takes).
 */
export interface FileProps extends CommonProps {
  name?: string; kind?: FileKind; size?: number; modified?: string | number | Date; value?: unknown; onclick?: Handle;
  path?: FileSource; line?: number;
}
/**
 * A cell of this lane shown inline, in a plate with its name and whether it runs or failed: the UI it drew, live, or its printed
 * output. `task` is the id the cell's reply gave it (`'t3'` or `3`); naming a cell this lane did not run is an error.
 */
export interface CellRefProps extends CommonProps { task: string | number; title?: string }
/**
 * The agent as the app shows it: Kip, the voxel mascot, standing at the column's left edge. It plays its moods in turn the first time
 * it is seen live (a reply read from history shows the last one), again when the pointer comes over it, and rests off-screen.
 */
export interface KipProps extends CommonProps {
  /** `'wave'`, `'happy'`, `'celebrate'`, `'thinking'`, `'working'`, `'oops'`, `'sleepy'`, `'idle'`; several (`'wave then idle'` or a list) play in turn. Waves when not given. */
  mood?: string | string[];
  /** How tall Kip stands, in points: 20 to 160, 64 when not given. */
  size?: number;
}
/** A width over a height: a number, `"16:9"` or `"16/9"`. */
export type Aspect = number | string;
/**
 * A video from a file or an http(s) URL, played by the app with `controls` (true: play/pause, a scrub bar, mute) shown on hover
 * and while paused. Muted unless `muted={false}`. `paused` and `currentTime` are commands (both bind); `ontimeupdate` gets the
 * seconds about 4 times a second while playing. Its `id` is what a Shader's '@id' names (its frames as a texture, its time as a
 * uniform).
 */
export interface VideoProps extends CommonProps {
  src: FileSource; poster?: FileSource; autoplay?: boolean; loop?: boolean; muted?: boolean; controls?: boolean;
  width?: number; height?: number; aspect?: Aspect; fit?: 'contain' | 'cover'; id?: string; paused?: boolean; currentTime?: number;
  ontimeupdate?: Handle<number>; onended?: Handle<null>; onplay?: Handle<null>; onpause?: Handle<null>;
}
/** A uniform's value: a number, a flag (1/0), 2 to 4 numbers, a colour `#rgb`..`#rrggbbaa`, or '@id' of a Slider, NumberField, Toggle, Checkbox or Video. */
export type UniformValue = number | boolean | readonly number[] | string;
/**
 * A Slang fragment shader, `float4 main(ShaderInput in)` (or Shadertoy's `mainImage`), compiled by the kernel and drawn by the
 * app every frame: its source between its tags, as `source`, or a `.slang` file as `src`. `uniforms` are read as `in.u.<name>`;
 * `textures` (an image or video file, an http(s) URL, or '@id' of a Video, Image or Shader) with `texture(name, uv)`.
 */
export interface ShaderProps extends CommonProps {
  source?: string; src?: FileSource; width?: number; height?: number; aspect?: Aspect;
  uniforms?: Readonly<Record<string, UniformValue>>; textures?: Readonly<Record<string, FileSource>>; animate?: boolean; id?: string;
}

/**
 * A page of the harness browser (embedded Chromium the daemon runs), live: `view` is a page an agent opened (`page.view` of
 * `browser.open(url)` or `withHiddenPage`), `src` an http(s) URL the app opens a view at; exactly one of them. Pressing it opens
 * the view in the browser pane (cmd+shift+B).
 */
export interface BrowserProps extends CommonProps { view?: string; src?: string }

/** Every built-in element's props, by element name. */
export interface ElementProps {
  Column: ColumnProps; Row: RowProps; Text: TextProps; Markdown: MarkdownProps; Code: CodeProps; Table: TableProps;
  Badge: BadgeProps; Stat: StatProps; Card: CardProps; Divider: DividerProps; Spacer: SpacerProps; Chart: ChartProps;
  Canvas: CanvasProps; Group: GroupProps; Path: PathProps; Rect: RectProps; Circle: CircleProps; Line: LineProps; Polyline: PolylineProps;
  Button: ButtonProps; Toggle: ToggleProps; Checkbox: CheckboxProps; Slider: SliderProps; NumberField: NumberFieldProps; Input: InputProps;
  TextArea: TextAreaProps; Select: SelectProps; Choice: ChoiceProps; Radio: RadioProps; ColorPicker: ColorPickerProps; DropZone: DropZoneProps;
  Grid: GridProps; Stack: StackProps; Layer: LayerProps; Wrap: WrapProps; Scroll: ScrollProps; Split: SplitProps; Tabs: TabsProps;
  Accordion: AccordionProps; Disclosure: DisclosureProps; Popover: PopoverProps; Menu: MenuProps; Dialog: DialogProps; Sheet: SheetProps;
  Toast: ToastProps; Progress: ProgressProps; Loading: LoadingProps; Skeleton: SkeletonProps; Icon: IconProps; Image: ImageProps;
  Chip: ChipProps; Tile: TileProps; FileRef: FileRefProps; File: FileProps; CellRef: CellRefProps; Kip: KipProps; Video: VideoProps; Shader: ShaderProps;
  Browser: BrowserProps;
}

/** True when ElementProps names exactly the elements element.ts draws; the constant under it fails to type-check otherwise. */
export type ElementPropsMatch = [Exclude<keyof ElementProps, ElementName>, Exclude<ElementName, keyof ElementProps>] extends [never, never] ? true : never;
export const ELEMENT_PROPS_MATCH: ElementPropsMatch = true;
