import type { ElementName } from './element.ts';

/*
 * The props of every element a cell can write as Svelte markup, as types. Events are lowercase, `onselect`, as Svelte 5 writes
 * them, and go to the harness app as `onSelect`. Cells are not type-checked (their types are erased), so these are the reference
 * an editor, a test or a component library written in TypeScript checks against; element.ts checks the same rules at run time
 * (PROPS and its rules), and the app draws them (harness-native/src/app/chat/genui.rs). Cut down from den's repl/src/ui/types.ts
 * to the elements the harness app draws: of den's props every element takes, only `tip` is kept (and `gap`, `pad`, `width`,
 * `height` where an element lists them).
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

/** The props every element takes besides its own. */
export interface CommonProps {
  /** Words shown when the pointer rests on it. */
  tip?: string;
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
}
export interface MarkdownProps extends CommonProps { text?: string }
export interface CodeProps extends CommonProps { text?: string; lang?: string }
export interface TableProps extends CommonProps { columns?: Array<string | { title: string; key?: string }>; rows?: unknown[] }
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
export interface CanvasProps extends CommonProps { view: string | [number, number, number, number]; width?: number; height?: number }
/** What every shape takes: its inks (a fill of ink when not said, unless it is an open line; no stroke when not said), how wide its stroke is and how opaque each. */
export interface ShapeProps extends CommonProps { fill?: Ink; stroke?: Ink; strokeWidth?: number; fillOpacity?: number; strokeOpacity?: number; transform?: Transform }
/** Shapes moved, scaled and turned together by `transform`, faded together by `opacity`. */
export interface GroupProps extends CommonProps { transform?: Transform; opacity?: number }
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
/** A press. Its label is `label`, or the words between its tags (`<Button>Save</Button>`); its plate is plain unless `tone` says. */
export interface ButtonProps { label?: string; tone?: 'accent' | 'error' | 'success' | 'warning' | 'muted'; disabled?: boolean; onclick?: Handle<null> }
/** A switch, on or off. Binds `value`. */
export interface ToggleProps { id?: string; value?: boolean; label?: string; disabled?: boolean; onchange?: Handle<boolean> }
/** A box ticked or not. Binds `checked`. */
export interface CheckboxProps { id?: string; checked?: boolean; label?: string; disabled?: boolean; onchange?: Handle<boolean> }
/** A number dragged from `min` (0) to `max` (100), snapped to `step`. Binds `value` (`min` when not said). */
export interface SliderProps { id?: string; value?: number; min?: number; max?: number; step?: number; label?: string; disabled?: boolean; onchange?: Handle<number> }
/** A number typed or stepped by `step` (1) within `min` and `max`. Binds `value` (`min`, else 0, when not said). */
export interface NumberFieldProps { id?: string; value?: number; min?: number; max?: number; step?: number; label?: string; disabled?: boolean; onchange?: Handle<number> }
/** One line of words; `onchange` hears them while typing (at most every 150 ms) and on leaving, `onsubmit` on Enter. Binds `value`. */
export interface InputProps { value?: string; placeholder?: string; label?: string; disabled?: boolean; onchange?: Handle<string>; onsubmit?: Handle<string> }
/** Words over `lines` (3) lines; `onsubmit` on Cmd+Enter. Binds `value`. */
export interface TextAreaProps { value?: string; placeholder?: string; lines?: number; disabled?: boolean; onchange?: Handle<string>; onsubmit?: Handle<string> }
/** An option: words (its label and its value), or a label with any JSON value. At most 200 options. */
export type ControlOption = string | number | { label: string; value?: unknown };
/** A field that drops its options to pick one. Binds `selected` (the picked option's value, null when none). */
export interface SelectProps { options: ControlOption[]; selected?: unknown; placeholder?: string; disabled?: boolean; onchange?: Handle }
/** Options side by side as segments, one picked. Binds `selected`. */
export interface ChoiceProps { options: ControlOption[]; selected?: unknown; disabled?: boolean; onchange?: Handle }
/** Options each with its round mark, one picked, in a column (or a `row`). Binds `selected`. */
export interface RadioProps { options: ControlOption[]; selected?: unknown; row?: boolean; disabled?: boolean; onchange?: Handle }

/*
 * Layout, folding, floating, status and pictures. Each goes to the app as exactly what element.ts `elementWire` gives it, defaults
 * filled in; the bindable ones (`bind:ratio`, `bind:selected`, `bind:open`) are written back from their event first, then the handler runs.
 */
/** Equal columns, row-major: `columns` of them (2 when neither is said), or as many as `fit` (each at least that many points wide). Gap 12. */
export interface GridProps { columns?: number; fit?: number; gap?: number; pad?: number }
/** Children on top of one another in one box, as big as its largest child that is not a Layer. */
export interface StackProps {}
/** Where a Layer is pinned in its Stack. */
export type LayerAnchor = 'top-left' | 'top' | 'top-right' | 'left' | 'center' | 'right' | 'bottom-left' | 'bottom' | 'bottom-right';
/** A child of a Stack (only there) pinned at `anchor` (top-left), moved by `x`, `y` points; a higher `z` is above. It adds no size. */
export interface LayerProps { anchor?: LayerAnchor; x?: number; y?: number; z?: number }
/** Children left to right, wrapping to the next line; gap 8. */
export interface WrapProps { gap?: number }
/** Its children in a window `height` (240, at most 600) points tall, or wide when `horizontal`, that scrolls with the wheel. */
export interface ScrollProps { height?: number; horizontal?: boolean }
/** Exactly two panes, side by side (or `vertical`ly), the first `ratio` (0.05 to 0.95, 0.5) of the room; dragging the divider sends the new ratio. Binds `ratio`. */
export interface SplitProps { ratio?: number; vertical?: boolean; onchange?: Handle<number> }
/** A row of 1 to 20 tabs over its children; the cell shows the selected one's body with {#if}. A press sends the tab's value. Binds `selected` (the first tab's value when not said). */
export interface TabsProps { tabs: ControlOption[]; selected?: unknown; onchange?: Handle }
/** Disclosures only, at most one open: opening one closes the open one (each hears `ontoggle`). */
export interface AccordionProps {}
/** A head of `title` that shows its children when open; a press on the head sends `!open`. Binds `open`. */
export interface DisclosureProps { title: string; open?: boolean; ontoggle?: Handle<boolean> }
/** Its first child is the trigger, drawn in place; the rest float on `placement` side (bottom) while open. A press on the trigger toggles it; a press outside or Escape sends false. Binds `open`. */
export interface PopoverProps { placement?: 'top' | 'bottom' | 'left' | 'right'; open?: boolean; ontoggle?: Handle<boolean> }
/** A row of a Menu: words (its label and its value), '-' (a separator), or a row with a value, a red `danger`, `disabled`, and the `keys` it shows. */
export type MenuItem = string | number | '-' | { separator: true } | { label: string; value?: unknown; danger?: boolean; disabled?: boolean; keys?: string };
/** 1 to 50 rows under its one child, the trigger. Picking a row sends `onselect` with its value and closes it. Binds `open`. */
export interface MenuProps { items: MenuItem[]; open?: boolean; onselect?: Handle; ontoggle?: Handle<boolean> }
/** A modal over a scrim covering the pane. When `dismissable` (true), Escape or a press on the scrim sends `onclose`, and `bind:open` becomes false. A Sheet rises from the pane's foot. */
export interface DialogProps { title?: string; open?: boolean; dismissable?: boolean; onclose?: Handle<false> }
export interface SheetProps { title?: string; open?: boolean; dismissable?: boolean; onclose?: Handle<false> }
/** Words floating at the pane's foot for `duration` ms (4000; 0 stays), gone when pressed (`ondismiss`); shown again whenever its words change. Its text is also its children. */
export interface ToastProps { text?: string; tone?: Tone; duration?: number; ondismiss?: Handle<null> }
/** A bar `value` (0) of `max` (1) full, with an optional `label`. */
export interface ProgressProps { value?: number; max?: number; label?: string }
/** A spinner and quiet words (`Loading`); its label is also its children. */
export interface LoadingProps { label?: string }
/** `lines` (3, 1 to 20) grey bars that breathe, where text is on its way. */
export interface SkeletonProps { lines?: number }
/** A file kind (`rust`, `markdown`) or symbol by `name`, from the app's file palette and symbol set; an unknown name is drawn as an error. */
export interface IconProps { name: string }
/** A file on disk (a path, relative ones from the working directory, `~/` the home directory, or a `{path}`), as the app is given it: an absolute path. */
export type FileSource = string | URL | Blob | { path: string };
/** A picture from a file (no http or data: URLs). Natural size, at most the column wide and 360 tall, unless `width`/`height` say; `fit` (contain) in a box of both. */
export interface ImageProps { src: FileSource; id?: string; width?: number; height?: number; fit?: 'contain' | 'cover'; alt?: string }
/** A pill, brighter when `selected`. A press sends `value`, or `!selected` when it has none. Its label is also its children. */
export interface ChipProps { label?: string; selected?: boolean; value?: unknown; onclick?: Handle }
/** Its children in a column, pressed as a whole (it lights under the pointer); a press sends `value`. Gap 8, pad 12. */
export interface TileProps { gap?: number; pad?: number; value?: unknown; onclick?: Handle }
/** A file chip, its kind's icon and its name; a press opens the file (at `line`) as the transcript's file chips do. */
export interface FileRefProps { path: FileSource; line?: number }
/**
 * The agent as the app shows it: Kip, the voxel mascot, standing at the column's left edge. It plays its moods in turn the first time
 * it is seen live (a reply read from history shows the last one), again when the pointer comes over it, and rests off-screen.
 */
export interface KipProps {
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
export interface VideoProps {
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
export interface ShaderProps {
  source?: string; src?: FileSource; width?: number; height?: number; aspect?: Aspect;
  uniforms?: Readonly<Record<string, UniformValue>>; textures?: Readonly<Record<string, FileSource>>; animate?: boolean; id?: string;
}

/** Every built-in element's props, by element name. */
export interface ElementProps {
  Column: ColumnProps; Row: RowProps; Text: TextProps; Markdown: MarkdownProps; Code: CodeProps; Table: TableProps;
  Badge: BadgeProps; Stat: StatProps; Card: CardProps; Divider: DividerProps; Spacer: SpacerProps; Chart: ChartProps;
  Canvas: CanvasProps; Group: GroupProps; Path: PathProps; Rect: RectProps; Circle: CircleProps; Line: LineProps; Polyline: PolylineProps;
  Button: ButtonProps; Toggle: ToggleProps; Checkbox: CheckboxProps; Slider: SliderProps; NumberField: NumberFieldProps; Input: InputProps;
  TextArea: TextAreaProps; Select: SelectProps; Choice: ChoiceProps; Radio: RadioProps;
  Grid: GridProps; Stack: StackProps; Layer: LayerProps; Wrap: WrapProps; Scroll: ScrollProps; Split: SplitProps; Tabs: TabsProps;
  Accordion: AccordionProps; Disclosure: DisclosureProps; Popover: PopoverProps; Menu: MenuProps; Dialog: DialogProps; Sheet: SheetProps;
  Toast: ToastProps; Progress: ProgressProps; Loading: LoadingProps; Skeleton: SkeletonProps; Icon: IconProps; Image: ImageProps;
  Chip: ChipProps; Tile: TileProps; FileRef: FileRefProps; Kip: KipProps; Video: VideoProps; Shader: ShaderProps;
}

/** True when ElementProps names exactly the elements element.ts draws; the constant under it fails to type-check otherwise. */
export type ElementPropsMatch = [Exclude<keyof ElementProps, ElementName>, Exclude<ElementName, keyof ElementProps>] extends [never, never] ? true : never;
export const ELEMENT_PROPS_MATCH: ElementPropsMatch = true;
