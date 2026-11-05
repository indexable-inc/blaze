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
 * binding nor a handler only shows its value. On the wire (element.ts `controlWire`) a handler is its id as `onClick`, `onChange`,
 * `onSubmit`, and defaults are filled in.
 */
/** A press. Its label is `label`, or the words between its tags (`<Button>Save</Button>`); its plate is plain unless `tone` says. */
export interface ButtonProps { label?: string; tone?: 'accent' | 'error' | 'success' | 'warning' | 'muted'; disabled?: boolean; onclick?: Handle<null> }
/** A switch, on or off. Binds `value`. */
export interface ToggleProps { value?: boolean; label?: string; disabled?: boolean; onchange?: Handle<boolean> }
/** A box ticked or not. Binds `checked`. */
export interface CheckboxProps { checked?: boolean; label?: string; disabled?: boolean; onchange?: Handle<boolean> }
/** A number dragged from `min` (0) to `max` (100), snapped to `step`. Binds `value` (`min` when not said). */
export interface SliderProps { value?: number; min?: number; max?: number; step?: number; label?: string; disabled?: boolean; onchange?: Handle<number> }
/** A number typed or stepped by `step` (1) within `min` and `max`. Binds `value` (`min`, else 0, when not said). */
export interface NumberFieldProps { value?: number; min?: number; max?: number; step?: number; label?: string; disabled?: boolean; onchange?: Handle<number> }
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

/** Every built-in element's props, by element name. */
export interface ElementProps {
  Column: ColumnProps; Row: RowProps; Text: TextProps; Markdown: MarkdownProps; Code: CodeProps; Table: TableProps;
  Badge: BadgeProps; Stat: StatProps; Card: CardProps; Divider: DividerProps; Spacer: SpacerProps; Chart: ChartProps;
  Canvas: CanvasProps; Group: GroupProps; Path: PathProps; Rect: RectProps; Circle: CircleProps; Line: LineProps; Polyline: PolylineProps;
  Button: ButtonProps; Toggle: ToggleProps; Checkbox: CheckboxProps; Slider: SliderProps; NumberField: NumberFieldProps; Input: InputProps;
  TextArea: TextAreaProps; Select: SelectProps; Choice: ChoiceProps; Radio: RadioProps;
}

/** True when ElementProps names exactly the elements element.ts draws; the constant under it fails to type-check otherwise. */
export type ElementPropsMatch = [Exclude<keyof ElementProps, ElementName>, Exclude<ElementName, keyof ElementProps>] extends [never, never] ? true : never;
export const ELEMENT_PROPS_MATCH: ElementPropsMatch = true;
