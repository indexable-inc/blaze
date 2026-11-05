/*
 * The model of a cell's UI: a tree of `ViewNode`s, the plain `{type, props, children}` shape the native app draws once it is
 * sent as JSON (harness-native/src/app/chat/genui.rs). Svelte (svelte.ts) builds and updates the tree through its custom
 * renderer; this file is only the data: the element names, the checks on their props, a node that knows its handlers, and the
 * brand of a component `render` can mount. The props each element reads and what each must be are here too (types.ts has them as
 * types), and every check says the cell line it is about when it is known. Nothing here depends on Svelte. Copied from den's repl/src/ui/element.ts and cut to
 * the elements the harness app draws.
 */
import { fileProblem } from './media.ts';

/** The element names the app draws (genui.rs `ELEMENTS`, one `hn.Gen<Name>` prefab each in scene/widgets/genui.flecs). */
export const ELEMENT_NAMES = [
  'Column', 'Row', 'Text', 'Markdown', 'Code', 'Table', 'Badge', 'Stat', 'Card', 'Divider', 'Spacer',
  // Charts, and drawing: shapes in a Canvas's own coordinates, in the scene's inks (draw.ts maps SVG onto these).
  'Chart', 'Canvas', 'Group', 'Path', 'Rect', 'Circle', 'Line', 'Polyline',
  // Controls: what the person presses, drags, types and picks; each sends its event back by handler id (index.ts `dispatch`).
  'Button', 'Toggle', 'Checkbox', 'Slider', 'NumberField', 'Input', 'TextArea', 'Select', 'Choice', 'Radio',
  // Layout: equal columns, children over one another (a Layer pinned in a Stack), a flow that wraps, a window that scrolls, two panes.
  'Grid', 'Stack', 'Layer', 'Wrap', 'Scroll', 'Split',
  // Folding and tabs: the cell picks what shows under the tab row with {#if}; an Accordion keeps one Disclosure open.
  'Tabs', 'Accordion', 'Disclosure',
  // Floating: drawn over the transcript, clipped to the pane.
  'Popover', 'Menu', 'Dialog', 'Sheet', 'Toast',
  // Status: a bar, a spinner with quiet words, grey lines where text is on its way.
  'Progress', 'Loading', 'Skeleton',
  // Pictures, chips and files: a file kind or symbol, a picture on disk, a pill, a column pressed as a whole, a file chip that opens.
  'Icon', 'Image', 'Chip', 'Tile', 'FileRef',
] as const;
export type ElementName = typeof ELEMENT_NAMES[number];

const known = new Set<string>(ELEMENT_NAMES);
export const isKnownElement = (name: string): name is ElementName => known.has(name);

/** Den's elements the harness app does not draw (yet): naming one is this error, never a blank or a ReferenceError. */
export const NOT_DRAWN: ReadonlySet<string> = new Set(['Kip', 'Browser', 'Shader', 'Video']);
export const notDrawnError = (name: string, where?: string): TypeError =>
  new TypeError(`<${name}> is not drawn by the harness app${where ? ` (${where})` : ''}: the elements are ${ELEMENT_NAMES.join(', ')}`);

/** The shapes, and what holds them: a Canvas draws Groups, shapes and Text, and nothing else does. */
export const SHAPES: ReadonlySet<string> = new Set(['Path', 'Rect', 'Circle', 'Line', 'Polyline']);
export const DRAWING: ReadonlySet<string> = new Set(['Group', ...SHAPES]);

/** What every shape takes besides its geometry. */
const SHAPE_PROPS = ['fill', 'stroke', 'strokeWidth', 'fillOpacity', 'strokeOpacity', 'transform', 'tip'] as const;

/**
 * The props each element reads (types.ts says what each is), its events as Svelte writes them (`onselect`), and `children` when
 * markup between its tags means something. A prop not here is an error naming the element and what it takes: Svelte would
 * pass it on and nothing would read it, so `<Code code={src} />` would draw an empty plate.
 */
export const PROPS: Record<ElementName, readonly string[]> = {
  Column: ['gap', 'pad', 'tip', 'children'],
  Row: ['gap', 'pad', 'tip', 'children'],
  // x, y, anchor, size and fill are a Text's inside a Canvas (or <svg>) only (compile.ts says so at the line).
  Text: ['text', 'tone', 'bold', 'x', 'y', 'anchor', 'size', 'fill', 'tip', 'children'],
  Markdown: ['text', 'tip', 'children'],
  Code: ['text', 'lang', 'tip', 'children'],
  Table: ['columns', 'rows', 'tip'],
  Badge: ['text', 'tone', 'tip', 'children'],
  Stat: ['label', 'value', 'note', 'tone', 'tip'],
  Card: ['title', 'gap', 'pad', 'tip', 'children'],
  Divider: ['tip'],
  Spacer: ['size', 'tip'],
  Chart: ['kind', 'data', 'labels', 'series', 'height', 'unit', 'center', 'caption', 'hole', 'pop', 'sep', 'fillOpacity', 'tone', 'legend', 'onselect', 'tip'],
  Canvas: ['view', 'width', 'height', 'tip', 'children'],
  Group: ['transform', 'opacity', 'tip', 'children'],
  Path: ['d', 'fillRule', ...SHAPE_PROPS],
  Rect: ['x', 'y', 'width', 'height', 'radius', ...SHAPE_PROPS],
  Circle: ['cx', 'cy', 'r', 'rx', 'ry', ...SHAPE_PROPS],
  Line: ['x1', 'y1', 'x2', 'y2', ...SHAPE_PROPS],
  Polyline: ['points', 'closed', ...SHAPE_PROPS],
  // The controls take exactly what the app reads of them (CONTROLS below says what goes on the wire), so no tip.
  Button: ['label', 'tone', 'disabled', 'onclick', 'children'],
  Toggle: ['value', 'label', 'disabled', 'onchange'],
  Checkbox: ['checked', 'label', 'disabled', 'onchange'],
  Slider: ['value', 'min', 'max', 'step', 'label', 'disabled', 'onchange'],
  NumberField: ['value', 'min', 'max', 'step', 'label', 'disabled', 'onchange'],
  Input: ['value', 'placeholder', 'label', 'disabled', 'onchange', 'onsubmit'],
  TextArea: ['value', 'placeholder', 'lines', 'disabled', 'onchange', 'onsubmit'],
  Select: ['options', 'selected', 'placeholder', 'disabled', 'onchange'],
  Choice: ['options', 'selected', 'disabled', 'onchange'],
  Radio: ['options', 'selected', 'row', 'disabled', 'onchange'],
  // Layout, folding, floating, status and pictures: each takes exactly what the app reads of it (`elementWire` says what goes), so no tip.
  Grid: ['columns', 'fit', 'gap', 'pad', 'children'],
  Stack: ['children'],
  Layer: ['anchor', 'x', 'y', 'z', 'children'],
  Wrap: ['gap', 'children'],
  Scroll: ['height', 'horizontal', 'children'],
  Split: ['ratio', 'vertical', 'onchange', 'children'],
  Tabs: ['tabs', 'selected', 'onchange', 'children'],
  Accordion: ['children'],
  Disclosure: ['title', 'open', 'ontoggle', 'children'],
  Popover: ['placement', 'open', 'ontoggle', 'children'],
  Menu: ['items', 'open', 'onselect', 'ontoggle', 'children'],
  Dialog: ['title', 'open', 'dismissable', 'onclose', 'children'],
  Sheet: ['title', 'open', 'dismissable', 'onclose', 'children'],
  Toast: ['text', 'tone', 'duration', 'ondismiss', 'children'],
  Progress: ['value', 'max', 'label'],
  Loading: ['label', 'children'],
  Skeleton: ['lines'],
  Icon: ['name'],
  Image: ['src', 'width', 'height', 'fit', 'alt'],
  Chip: ['label', 'selected', 'value', 'onclick', 'children'],
  Tile: ['gap', 'pad', 'value', 'onclick', 'children'],
  FileRef: ['path', 'line'],
};
/** The props only a Text inside a Canvas reads. */
export const CANVAS_TEXT_PROPS: ReadonlySet<string> = new Set(['x', 'y', 'anchor', 'size', 'fill']);

export type Tone = 'ink' | 'muted' | 'strong' | 'error' | 'accent' | 'success' | 'warning';
/** Every tone, as a record so the type and this list cannot drift apart. */
const TONE_SET: Record<Tone, true> = { ink: true, muted: true, strong: true, error: true, accent: true, success: true, warning: true };
export const TONES = Object.keys(TONE_SET) as Tone[];
export const isTone = (v: unknown): v is Tone => typeof v === 'string' && Object.hasOwn(TONE_SET, v);
/** The elements that take a `tone`. */
export const TONED: ReadonlySet<string> = new Set(['Text', 'Badge', 'Stat', 'Chart', 'Toast']);

/** The inks a shape's `fill` and `stroke` take (types.ts `Ink`): a tone, the quiet `faint`, a chart's series inks, or `none`. */
export const INKS: readonly string[] = [...TONES, 'faint', ...Array.from({ length: 8 }, (_, i) => `series${i + 1}`), 'none'];
export const isInk = (v: unknown): v is string => typeof v === 'string' && INKS.includes(v);
/** The inks as an error lists them. */
const INKS_SAID = `${TONES.join(', ')}, faint, series1..series8, none (series1.. for categorical data)`;
export const CHART_KINDS = ['bar', 'line', 'pie', 'donut', 'area', 'scatter', 'stacked'] as const;
/** The most series a Chart draws, and the most points in one. */
export const MAX_SERIES = 8;
export const MAX_POINTS = 2000;
/** The most options a Select, Choice or Radio offers, the longest label a control or an option shows, and the most lines a TextArea is tall. */
export const MAX_OPTIONS = 200;
export const MAX_LABEL = 200;
export const MAX_LINES = 40;
/** The most tabs a Tabs shows, rows a Menu lists, columns a Grid has, lines a Skeleton is, and points a Scroll is tall. */
export const MAX_TABS = 20;
export const MAX_ITEMS = 50;
export const MAX_COLUMNS = 12;
export const MAX_SKELETON = 20;
export const MAX_SCROLL = 600;
/** Where a Layer is pinned in its Stack, the side a Popover floats on, and how an Image fills a box of both width and height. */
export const LAYER_ANCHORS = ['top-left', 'top', 'top-right', 'left', 'center', 'right', 'bottom-left', 'bottom', 'bottom-right'] as const;
export const PLACEMENTS = ['top', 'bottom', 'left', 'right'] as const;
export const IMAGE_FITS = ['contain', 'cover'] as const;
/** The tones a Button takes: its plate is plain when none is said. */
export const BUTTON_TONES = ['accent', 'error', 'success', 'warning', 'muted'] as const;

const said = (v: unknown): string => typeof v === 'string' || (typeof v === 'object' && v !== null) ? JSON.stringify(v) : String(v);
/** `name=value` as markup would say it: `fill="#fff"`, `width={-2}`. */
export const written = (name: string, value: unknown): string => {
  if (typeof value === 'string') return `${name}=${JSON.stringify(value)}`;
  const text = said(value);
  return `${name}={${text.length > 60 ? `${text.slice(0, 59)}…` : text}}`;
};
const at = (where?: string) => where ? ` (${where})` : '';

/**
 * The error for a `tone` on `element` that is not one, or undefined when it is one or none (`undefined`, `null`, `''`); `where` is the
 * cell's line, when it is known.
 */
export function toneError(element: string, value: unknown, where?: string): TypeError | undefined {
  if (value === undefined || value === null || value === '' || isTone(value)) return undefined;
  const shown = typeof value === 'string' ? JSON.stringify(value) : `{${String(value)}}`;
  return new TypeError(`tone=${shown} on <${element}> is not a tone${at(where)}: colour comes only from tone, one of ${TONES.join(', ')}`);
}

/** The error for a `fill` or `stroke` that is not an ink (a hex, `rgb()`, a colour name). `currentColor` is the ink (draw.ts). */
export function inkError(element: string, name: string, value: unknown, where?: string): TypeError | undefined {
  if (value === undefined || value === null || isInk(value) || value === 'currentColor') return undefined;
  return new TypeError(`${written(name, value)} on <${element}> is not an ink${at(where)}: colour comes from the scene's inks, one of ${INKS_SAID}`);
}

/**
 * The error for a Table's `columns` and `rows` when they are not what it reads, naming what is wrong; undefined when they are fine.
 * `rows` is a list of records (`[{name, cpu}]`) or of lists (`[['a', 1]]`); `columns`, when given, is a list of titles (or
 * `{title, key}`: the record field it shows, its title when not said).
 */
export function tableError(columns: unknown, rows: unknown, where?: string): TypeError | undefined {
  if (rows !== undefined && rows !== null && !Array.isArray(rows)) return new TypeError(`rows=${said(rows)} on <Table> is not a list${at(where)}: rows={[{name: 'a', cpu: 3}]}`);
  if (columns === undefined || columns === null) return undefined;
  if (!Array.isArray(columns)) return new TypeError(`columns=${said(columns)} on <Table> is not a list${at(where)}: columns={['name', 'cpu']}`);
  for (const [i, col] of columns.entries()) {
    if (typeof col === 'string') continue;
    if (typeof col !== 'object' || col === null || typeof (col as { title?: unknown }).title !== 'string') {
      return new TypeError(`<Table> column ${i} is ${said(col)}${at(where)}: a column is a title or {title, key}`);
    }
  }
  return undefined;
}

/**
 * What a bindable element is: `<Slider bind:value={v}>` is the value the person sets, `<Choice bind:selected={s}>` the one they pick.
 * Its change event writes the prop back (svelte.ts `hostSource`), then the cell's own `onchange` runs, so a binding follows the person
 * and a plain `value={x}` still shows what is written. Copied from den's repl/src/ui/element.ts, cut to the app's controls.
 */
export const BINDABLE: Partial<Record<string, string>> = {
  Input: 'value', Slider: 'value', Toggle: 'value', NumberField: 'value', TextArea: 'value', Checkbox: 'checked',
  Choice: 'selected', Radio: 'selected', Select: 'selected',
  Split: 'ratio', Tabs: 'selected', Disclosure: 'open', Popover: 'open', Menu: 'open', Dialog: 'open', Sheet: 'open',
};

/**
 * The names a model writes for an element's binding out of HTML's and Svelte's habit (`bind:checked` on a switch, `bind:value` on a
 * select), each taken as the binding itself: compile.ts rewrites `bind:checked` on a Toggle to `bind:value`, and a plain `checked={x}`
 * to `value={x}`, so the cell works as written instead of failing on a name.
 */
export const BIND_ALIAS: Partial<Record<string, Readonly<Record<string, string>>>> = {
  Toggle: { checked: 'value' }, Checkbox: { value: 'checked' }, Select: { value: 'selected' }, Choice: { value: 'selected' },
  Radio: { value: 'selected', group: 'selected' }, Tabs: { value: 'selected' },
};

/** An event that writes a binding back, and what it writes when not the event's value. */
export interface WriteBack { event: string; value?: unknown }
/**
 * The events that write a binding back when it is not `change` alone: a fold or a float is opened and shut by `toggle`, a Menu also shuts
 * when a row is picked, and closing a Dialog or a Sheet is `open = false`. Copied from den's repl/src/ui/element.ts (Menu added).
 */
export const BOUND_BY: Partial<Record<string, readonly WriteBack[]>> = {
  Disclosure: [{ event: 'toggle' }], Popover: [{ event: 'toggle' }], Menu: [{ event: 'toggle' }, { event: 'select', value: false }],
  Dialog: [{ event: 'close', value: false }], Sheet: [{ event: 'close', value: false }],
};

/**
 * The error for `bind:<name>` on the element `element` when it binds nothing, or undefined when it is the element's binding (or
 * `bind:this`); `where` is the cell's line. Svelte passes any other name on as a prop no element reads, and nothing writes it back,
 * so the variable would never follow the person.
 */
export function bindError(element: string, name: string, where?: string): TypeError | undefined {
  const bound = BINDABLE[element];
  if (name === 'this' || name === bound || BIND_ALIAS[element]?.[name] === bound) return undefined;
  if (bound) return new TypeError(`bind:${name} on <${element}> binds nothing${at(where)}: a ${element}'s binding is bind:${bound}={...}`);
  const takers = Object.entries(BINDABLE).map(([el, prop]) => `${el} (bind:${prop})`).join(', ');
  return new TypeError(`bind:${name} on <${element}> binds nothing${at(where)}: only ${takers} take a binding; pass ${name}={...} and an on-handler instead`);
}

/** The props whose value children text becomes: `<Text>hi</Text>` is `{text: 'hi'}`. */
export const TEXT_PROP: Partial<Record<string, string>> = {
  Text: 'text', Badge: 'text', Code: 'text', Markdown: 'text', Button: 'label', Chip: 'label', Loading: 'label', Toast: 'text',
};
/** The elements whose children are only their words (`<Chip>Open</Chip>`): another element between their tags is an error. */
export const WORDS_ONLY: ReadonlySet<string> = new Set(['Button', 'Chip', 'Loading', 'Toast']);

export const article = (word: string): string => /^[AEIOU]/.test(word) ? 'an' : 'a';
const squash = (name: string) => name.toLowerCase().replace(/[-_:]/g, '');

/** What an element takes, as an error says it: `a Code takes text, lang (its text is also its children)`. */
export function takes(element: ElementName): string {
  const props = PROPS[element].filter(name => name !== 'children' && name !== 'tip');
  const list = props.length ? `takes ${props.join(', ')}` : 'takes only tip';
  const kids = !PROPS[element].includes('children') ? '' : TEXT_PROP[element] ? ` (its ${TEXT_PROP[element]} is also its children)` : ' (and children)';
  return `${article(element)} ${element} ${list}${kids}`;
}

/** The error for a prop `name` that `element` does not read, with the one it may have meant. */
export function unknownPropError(element: ElementName, name: string, value: unknown, where?: string): TypeError {
  const meant = PROPS[element].find(prop => squash(prop) === squash(name) || squash(prop) === squash(name.replace(/^stroke-?/, 'stroke')));
  const hint = name === 'children' ? `${article(element)} ${element} takes no children: ${takes(element)}`
    : `${takes(element)}${meant ? `; did you mean ${meant}?` : ''}`;
  // As the markup wrote it: text as text, a bare attribute bare, anything else (an expression, an event, a spread's value) as `{...}`.
  const shown = name === 'children' ? '' : value === true ? ` ${name}` : typeof value === 'string' && !/^on/.test(name) ? ` ${written(name, value)}` : ` ${name}={...}`;
  return new TypeError(`<${element}${shown}> has no prop ${JSON.stringify(name)}${at(where)}: ${hint}`);
}

const numeric = (v: unknown): boolean => (typeof v === 'number' && Number.isFinite(v)) || (typeof v === 'string' && /^\s*[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?\s*$/.test(v));
const asNumber = (v: unknown): number => typeof v === 'number' ? v : Number(String(v).trim());
type Rule = (v: unknown) => string | undefined;
/** The rules whose value is a number: words that are one (`x="10"`, as markup writes it) go to the app as the number (`wireValue`). */
const NUMERIC = new Set<Rule>();
/** `test` as a rule of numbers: fine when it holds of the number, else `wants`. */
const numeral = (test: (n: number) => boolean, wants: string): Rule => {
  const rule: Rule = v => numeric(v) && test(asNumber(v)) ? undefined : wants;
  NUMERIC.add(rule);
  return rule;
};
const whole = (lo: number, hi: number, what: string): Rule => numeral(n => Number.isInteger(n) && n >= lo && n <= hi, `a whole number of ${what}, ${lo} to ${hi}`);
const oneOf = (list: readonly string[]): Rule => v => typeof v === 'string' && list.includes(v) ? undefined : `one of ${list.join(', ')}`;
const number: Rule = numeral(() => true, 'a number');
const share: Rule = numeral(n => n >= 0 && n <= 1, 'a number from 0 to 1');
const size: Rule = numeral(n => n >= 0, 'a number of points, 0 or more');
const words: Rule = v => typeof v === 'string' || (typeof v === 'number' && Number.isFinite(v)) ? undefined : 'words (a string)';
const flag: Rule = v => typeof v === 'boolean' || v === 'true' || v === 'false' ? undefined : 'true or false';
const ink: Rule = v => isInk(v) || v === 'currentColor' ? undefined : `an ink: ${INKS_SAID}`;
/** A list of numbers, from `"0,0 10,5"`, `[0, 0, 10, 5]` or `[[0, 0], [10, 5]]`; undefined when it is none of those. */
export const numbers = (v: unknown): number[] | undefined => typeof v === 'string' ? v.trim().split(/[\s,]+/).filter(Boolean).map(Number)
  : Array.isArray(v) ? v.flat().map(n => typeof n === 'number' ? n : typeof n === 'string' && numeric(n) ? Number(n) : NaN) : undefined;
const affine: Rule = v => typeof v === 'string' || (Array.isArray(v) && v.length === 6 && v.every(numeric)) ? undefined
  : `an SVG transform, "translate(10 20) rotate(45)", or six numbers [a, b, c, d, e, f]`;

const label: Rule = v => (typeof v === 'string' || (typeof v === 'number' && Number.isFinite(v))) && String(v).length <= MAX_LABEL ? undefined : `words (a string) of at most ${MAX_LABEL} characters`;
const text: Rule = v => typeof v === 'string' || (typeof v === 'number' && Number.isFinite(v)) ? undefined : 'words (a string)';
const positive: Rule = numeral(n => n > 0, 'a number above 0');
const lines: Rule = whole(1, MAX_LINES, 'lines');

/** One option of a Select, Choice or Radio, or one tab of a Tabs, as the app reads it. */
export interface WireOption { label: string; value: unknown }

/**
 * The problem with `option` as an option (index `i` of the list `what`, `options` or `tabs`), or the option as the app reads it: words
 * are both its label and its value.
 */
function optionOf(option: unknown, i: number, what = 'options'): WireOption | string {
  const one = what === 'tabs' ? 'a tab' : 'an option';
  if (typeof option === 'string' || (typeof option === 'number' && Number.isFinite(option))) {
    return String(option).length <= MAX_LABEL ? { label: String(option), value: option } : `${what}[${i}] is longer than ${MAX_LABEL} characters`;
  }
  if (typeof option !== 'object' || option === null || Array.isArray(option)) return `${what}[${i}] is ${said(option)}: ${one} is words or {label, value}`;
  const o = option as Record<string, unknown>;
  const stray = Object.keys(o).find(key => key !== 'label' && key !== 'value');
  if (stray) return `${what}[${i}] has ${JSON.stringify(stray)}: ${one} is words or {label, value}`;
  if (typeof o.label !== 'string' || o.label.length > MAX_LABEL) return `${what}[${i}] has label ${said(o.label)}: ${one}'s label is words of at most ${MAX_LABEL} characters`;
  return { label: o.label, value: 'value' in o && o.value !== undefined ? o.value : o.label };
}

/** The problem with a Tabs' `tabs`, or the tabs as the app reads them: 1 to MAX_TABS, each words or {label, value}. */
export function tabsOf(value: unknown): WireOption[] | string {
  if (!Array.isArray(value) || value.length === 0) return `a list of 1 to ${MAX_TABS} tabs, each words or {label, value}: tabs={['Overview', 'Logs']}`;
  if (value.length > MAX_TABS) return `at most ${MAX_TABS} tabs (it has ${value.length}): pick with a Select instead`;
  const out: WireOption[] = [];
  for (const [i, tab] of value.entries()) {
    const one = optionOf(tab, i, 'tabs');
    if (typeof one === 'string') return `a list of tabs: ${one}`;
    out.push(one);
  }
  return out;
}
const tabs: Rule = v => { const t = tabsOf(v); return typeof t === 'string' ? t : undefined; };

/** One row of a Menu as the app reads it: a separator, or a row that sends its value when picked. */
export type WireItem = { separator: true } | { label: string; value: unknown; danger?: boolean; disabled?: boolean; keys?: string };
const ITEM_SAID = `words, '-' (a separator) or {label, value, danger, disabled, keys}`;

/** The problem with `item` as a Menu row (index `i`), or the row as the app reads it. */
function itemOf(item: unknown, i: number): WireItem | string {
  if (item === '-') return { separator: true };
  if (typeof item === 'string' || (typeof item === 'number' && Number.isFinite(item))) {
    return String(item).length <= MAX_LABEL ? { label: String(item), value: item } : `items[${i}] is longer than ${MAX_LABEL} characters`;
  }
  if (typeof item !== 'object' || item === null || Array.isArray(item)) return `items[${i}] is ${said(item)}: a row is ${ITEM_SAID}`;
  const o = item as Record<string, unknown>;
  if (o.separator === true && Object.keys(o).length === 1) return { separator: true };
  const stray = Object.keys(o).find(key => !['label', 'value', 'danger', 'disabled', 'keys'].includes(key));
  if (stray) return `items[${i}] has ${JSON.stringify(stray)}: a row is ${ITEM_SAID}`;
  if (typeof o.label !== 'string' || o.label.length > MAX_LABEL) return `items[${i}] has label ${said(o.label)}: a row's label is words of at most ${MAX_LABEL} characters`;
  const row: WireItem = { label: o.label, value: o.value !== undefined ? o.value : o.label };
  for (const name of ['danger', 'disabled'] as const) {
    if (o[name] === undefined) continue;
    if (typeof o[name] !== 'boolean') return `items[${i}] has ${name} ${said(o[name])}: true or false`;
    row[name] = o[name];
  }
  if (o.keys !== undefined) {
    if (typeof o.keys !== 'string' || o.keys.length > 40) return `items[${i}] has keys ${said(o.keys)}: the shortcut it shows, as words ('⌘K')`;
    row.keys = o.keys;
  }
  return row;
}

/** The problem with a Menu's `items`, or the rows as the app reads them: 1 to MAX_ITEMS. */
export function itemsOf(value: unknown): WireItem[] | string {
  if (!Array.isArray(value) || value.length === 0) return `a list of 1 to ${MAX_ITEMS} rows, each ${ITEM_SAID}: items={['Rename', '-', {label: 'Delete', value: 'rm', danger: true}]}`;
  if (value.length > MAX_ITEMS) return `at most ${MAX_ITEMS} rows (it has ${value.length}): search them with an Input and a Table instead`;
  const out: WireItem[] = [];
  for (const [i, item] of value.entries()) {
    const one = itemOf(item, i);
    if (typeof one === 'string') return `a list of rows: ${one}`;
    out.push(one);
  }
  return out;
}
const items: Rule = v => { const r = itemsOf(v); return typeof r === 'string' ? r : undefined; };
const file: Rule = v => fileProblem(v);

/** The problem with a Select's, Choice's or Radio's `options`, or the options as the app reads them. */
export function optionsOf(value: unknown): WireOption[] | string {
  if (!Array.isArray(value)) return `a list of options, each words or {label, value}: options={['small', 'large']}`;
  if (value.length > MAX_OPTIONS) return `at most ${MAX_OPTIONS} options (it has ${value.length}): narrow them first, or show them as a Table`;
  const out: WireOption[] = [];
  for (const [i, option] of value.entries()) {
    const one = optionOf(option, i);
    if (typeof one === 'string') return `a list of options: ${one}`;
    out.push(one);
  }
  return out;
}
const options: Rule = v => { const o = optionsOf(v); return typeof o === 'string' ? o : undefined; };

/** What each prop must be, on every element; a prop not here (and not in `ELEMENT_RULES`) is not checked beyond being one it reads. */
const COMMON_RULES: Record<string, Rule> = { gap: size, pad: size, size, width: size, height: size, tip: words };

const SHAPE_RULES: Record<string, Rule> = {
  fill: ink, stroke: ink, strokeWidth: size, fillOpacity: share, strokeOpacity: share, transform: affine,
};
/** The anchors a Canvas's Text hangs from its point by. */
export const TEXT_ANCHORS = ['start', 'middle', 'end'] as const;

/** What a prop must be on one element. */
const ELEMENT_RULES: Partial<Record<ElementName, Record<string, Rule>>> = {
  Text: { bold: flag, x: number, y: number, anchor: oneOf(TEXT_ANCHORS), fill: ink },
  Chart: {
    kind: oneOf(CHART_KINDS), hole: share, pop: size, sep: size, fillOpacity: share, unit: words, center: words, caption: words, legend: flag,
    labels: v => Array.isArray(v) && v.every(l => typeof l === 'string' || typeof l === 'number') ? undefined : `a list of words, labels={['a', 'b']}`,
  },
  Canvas: {
    view: v => {
      const n = numbers(v);
      return n && n.length === 4 && n.every(Number.isFinite) && n[2]! > 0 && n[3]! > 0 ? undefined : `four numbers, minX minY width height, as SVG's viewBox: "0 0 100 50"`;
    },
  },
  Group: { transform: affine, opacity: share },
  Path: { ...SHAPE_RULES, fillRule: oneOf(['nonzero', 'evenodd']), d: v => typeof v === 'string' && /^[MmLlHhVvCcSsQqTtAaZz\d\s.,+eE-]*$/.test(v) ? undefined : `SVG path data, "M 0 0 L 10 10 Z" (M L H V C S Q T A Z, upper or lower case)` },
  Rect: { ...SHAPE_RULES, x: number, y: number, radius: size },
  Circle: { ...SHAPE_RULES, cx: number, cy: number, r: size, rx: size, ry: size },
  Line: { ...SHAPE_RULES, x1: number, y1: number, x2: number, y2: number },
  Polyline: {
    ...SHAPE_RULES, closed: flag,
    points: v => { const n = numbers(v); return n && n.length % 2 === 0 && n.every(Number.isFinite) ? undefined : `points as x,y pairs: [[0, 0], [10, 5]], [0, 0, 10, 5] or "0,0 10,5"`; },
  },
  Button: { label, tone: v => v === '' ? undefined : oneOf(BUTTON_TONES)(v), disabled: flag },
  Toggle: { value: flag, label, disabled: flag },
  Checkbox: { checked: flag, label, disabled: flag },
  Slider: { value: number, min: number, max: number, step: positive, label, disabled: flag },
  NumberField: { value: number, min: number, max: number, step: positive, label, disabled: flag },
  Input: { value: text, placeholder: label, label, disabled: flag },
  TextArea: { value: text, placeholder: label, lines, disabled: flag },
  Select: { options, placeholder: label, disabled: flag },
  Choice: { options, disabled: flag },
  Radio: { options, row: flag, disabled: flag },
  Grid: { columns: whole(1, MAX_COLUMNS, 'columns'), fit: numeral(n => n > 0, 'a number of points above 0 (the narrowest a column may be)') },
  Layer: { anchor: oneOf(LAYER_ANCHORS), x: number, y: number, z: number },
  Scroll: { height: numeral(n => n > 0 && n <= MAX_SCROLL, `a number of points above 0, at most ${MAX_SCROLL}`), horizontal: flag },
  Split: { ratio: numeral(n => n >= 0.05 && n <= 0.95, 'a share of the room from 0.05 to 0.95'), vertical: flag },
  Tabs: { tabs },
  Disclosure: { title: label, open: flag },
  Popover: { placement: oneOf(PLACEMENTS), open: flag },
  Menu: { items, open: flag },
  Dialog: { title: label, open: flag, dismissable: flag },
  Sheet: { title: label, open: flag, dismissable: flag },
  Toast: { text, duration: numeral(n => n >= 0, 'milliseconds, 0 or more (0 stays until it is pressed)') },
  Progress: { value: number, max: positive, label },
  Loading: { label },
  Skeleton: { lines: whole(1, MAX_SKELETON, 'lines') },
  Icon: { name: label },
  Image: { src: file, width: positive, height: positive, fit: oneOf(IMAGE_FITS), alt: label },
  Chip: { label, selected: flag },
  FileRef: { path: file, line: whole(1, Number.MAX_SAFE_INTEGER, 'a line') },
};

/** The elements that go to the app as exactly the props `elementWire` gives them: the controls, and every element of tier 3. */
export const WIRED: ReadonlySet<string> = new Set([
  'Button', 'Toggle', 'Checkbox', 'Slider', 'NumberField', 'Input', 'TextArea', 'Select', 'Choice', 'Radio',
  'Grid', 'Stack', 'Layer', 'Wrap', 'Scroll', 'Split', 'Tabs', 'Accordion', 'Disclosure', 'Popover', 'Menu', 'Dialog', 'Sheet', 'Toast',
  'Progress', 'Loading', 'Skeleton', 'Icon', 'Image', 'Chip', 'Tile', 'FileRef',
]);

/** Two values the same as JSON says them (a tab's value against the selected one). */
const same = (a: unknown, b: unknown): boolean => a === b || JSON.stringify(a) === JSON.stringify(b);

/**
 * An element's props as the app reads them (the harness app's genui.rs is written from this exact shape), once all are known:
 * defaults filled in, words that are numbers made numbers, options, tabs and rows as objects, an Image's or a FileRef's file as an
 * absolute path (tree.ts resolves it), and its handler ids as `onClick`, `onChange`, `onSubmit`, `onToggle`, `onSelect`, `onClose` and
 * `onDismiss`. Throws the error naming what is wrong (a Button with no label, a Slider whose min is not below its max, a Grid with both
 * columns and fit) at `where`.
 */
export function elementWire(element: string, props: Record<string, unknown>, where?: string): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  const given = (name: string) => props[name] !== undefined && props[name] !== null && props[name] !== '';
  const put = (...names: string[]) => { for (const name of names) if (given(name)) out[name] = props[name]; };
  const num = (name: string): number | undefined => given(name) ? asNumber(props[name]) : undefined;
  const yes = (name: string, otherwise: boolean): boolean => given(name) ? props[name] === true : otherwise;
  const fail = (problem: string, hint?: string) => new TypeError(`<${element}> ${problem}${at(where)}${hint ? `: ${hint}` : ''}`);
  /** The words of prop `name`, which the element cannot go without: `hint` says how to give them. */
  const needs = (name: string, hint: string): string => {
    if (!given(name) || String(props[name]).trim() === '') throw fail(`has no ${name}`, hint);
    return String(props[name]);
  };
  switch (element) {
    case 'Button': {
      if (!given('label') || String(props.label).trim() === '') throw fail('has no label', 'say label="Save", or write its words between its tags, <Button>Save</Button>');
      const words = String(props.label);
      if (words.length > MAX_LABEL) throw fail(`has a label of ${words.length} characters; a Button's is at most ${MAX_LABEL}`);
      out.label = words;
      put('tone', 'disabled', 'onClick');
      break;
    }
    case 'Toggle':
      out.value = props.value === true;
      put('label', 'disabled', 'onChange');
      break;
    case 'Checkbox':
      out.checked = props.checked === true;
      put('label', 'disabled', 'onChange');
      break;
    case 'Slider': {
      const min = num('min') ?? 0;
      const max = num('max') ?? 100;
      if (!(min < max)) throw fail(`has min=${min} and max=${max}`, `a Slider's min is below its max (0 and 100 when not said)`);
      out.value = num('value') ?? min;
      out.min = min;
      out.max = max;
      if (given('step')) out.step = num('step');
      put('label', 'disabled', 'onChange');
      break;
    }
    case 'NumberField': {
      const min = num('min');
      const max = num('max');
      if (min !== undefined && max !== undefined && min > max) throw fail(`has min=${min} and max=${max}`, 'its min is at most its max');
      out.value = num('value') ?? min ?? 0;
      if (min !== undefined) out.min = min;
      if (max !== undefined) out.max = max;
      out.step = num('step') ?? 1;
      put('label', 'disabled', 'onChange');
      break;
    }
    case 'Input':
    case 'TextArea':
      out.value = given('value') ? String(props.value) : '';
      put('placeholder');
      if (element === 'Input') put('label');
      else out.lines = num('lines') ?? 3;
      put('disabled', 'onChange', 'onSubmit');
      break;
    case 'Select':
    case 'Choice':
    case 'Radio': {
      if (!given('options')) throw fail('has no options', `options={['small', 'large']} (words, or {label, value})`);
      const list = optionsOf(props.options);
      if (typeof list === 'string') throw fail(`options are not ${list}`);
      out.options = list;
      out.selected = props.selected ?? null;
      if (element === 'Select') put('placeholder');
      if (element === 'Radio') put('row');
      put('disabled', 'onChange');
      break;
    }
    case 'Grid':
      if (given('columns') && given('fit')) throw fail('has both columns and fit', 'a Grid has columns={3} (that many equal columns) or fit={160} (as many as fit, each at least that wide), not both');
      if (given('fit')) out.fit = num('fit');
      else out.columns = num('columns') ?? 2;
      out.gap = num('gap') ?? 12;
      out.pad = num('pad') ?? 0;
      break;
    case 'Stack':
    case 'Accordion':
      break;
    case 'Layer':
      out.anchor = given('anchor') ? props.anchor : 'top-left';
      out.x = num('x') ?? 0;
      out.y = num('y') ?? 0;
      out.z = num('z') ?? 0;
      break;
    case 'Wrap':
      out.gap = num('gap') ?? 8;
      break;
    case 'Scroll':
      out.height = num('height') ?? 240;
      out.horizontal = yes('horizontal', false);
      break;
    case 'Split':
      out.ratio = num('ratio') ?? 0.5;
      out.vertical = yes('vertical', false);
      put('onChange');
      break;
    case 'Tabs': {
      if (!given('tabs')) throw fail('has no tabs', `tabs={['Overview', 'Logs']} (words, or {label, value}); the cell shows each tab's body with {#if}`);
      const list = tabsOf(props.tabs);
      if (typeof list === 'string') throw fail(`tabs are not ${list}`);
      const selected = given('selected') ? props.selected : list[0]!.value;
      if (!list.some(tab => same(tab.value, selected))) throw fail(`has selected=${said(selected)}, which is no tab's value`, `one of ${list.map(tab => said(tab.value)).join(', ')}`);
      out.tabs = list;
      out.selected = selected;
      put('onChange');
      break;
    }
    case 'Disclosure':
      out.title = needs('title', 'say title="Details": the head pressed to open it');
      out.open = yes('open', false);
      put('onToggle');
      break;
    case 'Popover':
      out.placement = given('placement') ? props.placement : 'bottom';
      out.open = yes('open', false);
      put('onToggle');
      break;
    case 'Menu': {
      if (!given('items')) throw fail('has no items', `items={['Rename', '-', {label: 'Delete', value: 'rm', danger: true}]}`);
      const list = itemsOf(props.items);
      if (typeof list === 'string') throw fail(`items are not ${list}`);
      out.items = list;
      out.open = yes('open', false);
      put('onSelect', 'onToggle');
      break;
    }
    case 'Dialog':
    case 'Sheet':
      if (given('title')) out.title = String(props.title);
      out.open = yes('open', false);
      out.dismissable = yes('dismissable', true);
      put('onClose');
      break;
    case 'Toast':
      out.text = needs('text', 'say text="Saved", or write its words between its tags, <Toast>Saved</Toast>');
      out.tone = given('tone') ? props.tone : 'ink';
      out.duration = num('duration') ?? 4000;
      put('onDismiss');
      break;
    case 'Progress': {
      const max = num('max') ?? 1;
      const value = num('value') ?? 0;
      if (value < 0 || value > max) throw fail(`has value=${value} and max=${max}`, 'its value is from 0 to its max (1 when not said)');
      out.value = value;
      out.max = max;
      if (given('label')) out.label = String(props.label);
      break;
    }
    case 'Loading':
    {
      const words = given('label') ? String(props.label) : 'Loading';
      if (words.length > MAX_LABEL) throw fail(`has a label of ${words.length} characters; a Loading's is at most ${MAX_LABEL}`);
      out.label = words;
      break;
    }
    case 'Skeleton':
      out.lines = num('lines') ?? 3;
      break;
    case 'Icon':
      out.name = needs('name', `say name="rust" (a file kind) or a symbol's name`);
      break;
    case 'Image':
      if (!given('src')) throw fail('has no src', 'src="/abs/path/picture.png" (a file on disk; a relative path is from the working directory)');
      out.src = props.src;
      put('width', 'height');
      out.fit = given('fit') ? props.fit : 'contain';
      if (given('alt')) out.alt = String(props.alt);
      break;
    case 'Chip': {
      // Words between its tags are not checked as they are set, so the cap is checked here, as a Button's is.
      const words = needs('label', 'say label="Open", or write its words between its tags, <Chip>Open</Chip>');
      if (words.length > MAX_LABEL) throw fail(`has a label of ${words.length} characters; a Chip's is at most ${MAX_LABEL}`);
      out.label = words;
      out.selected = yes('selected', false);
      if (props.value !== undefined && props.value !== null) out.value = props.value;
      put('onClick');
      break;
    }
    case 'Tile':
      out.gap = num('gap') ?? 8;
      out.pad = num('pad') ?? 12;
      if (props.value !== undefined && props.value !== null) out.value = props.value;
      put('onClick');
      break;
    case 'FileRef':
      if (!given('path')) throw fail('has no path', 'path="/abs/path/file.rs" (a relative path is from the working directory)');
      out.path = props.path;
      if (given('line')) out.line = num('line');
      break;
    default:
      throw new Error(`elementWire: <${element}> is not one of the elements sent exactly (WIRED)`);
  }
  return out;
}

/**
 * The error for what an element holds when it is not what the app draws of it, given the drawn children's types, or undefined: a Split
 * holds two panes, an Accordion only Disclosures, a Popover its trigger and then its float, a Menu only its trigger.
 */
export function holdsError(element: string, kids: readonly string[], where?: string): TypeError | undefined {
  const fail = (problem: string, hint: string) => new TypeError(`<${element}> ${problem}${at(where)}: ${hint}`);
  const count = (n: number) => n === 0 ? 'nothing' : n === 1 ? 'one child' : `${n} children`;
  switch (element) {
    case 'Split': return kids.length === 2 ? undefined : fail(`holds ${count(kids.length)}`, 'a Split holds exactly two, the panes either side of its divider');
    case 'Accordion': {
      const other = kids.find(kid => kid !== 'Disclosure');
      return other === undefined ? undefined : fail(`holds ${article(other)} ${other}`, 'an Accordion holds only Disclosures, <Disclosure title="...">...</Disclosure>, and keeps one open');
    }
    case 'Popover': return kids.length >= 1 ? undefined : fail('holds nothing', 'its first child is the trigger, drawn in place; the rest float by it while it is open');
    case 'Menu': return kids.length === 1 ? undefined : fail(`holds ${count(kids.length)}`, 'a Menu holds one child, its trigger; its rows are items={[...]}');
    default: return undefined;
  }
}

/** The error for an element where it cannot be, under a parent of type `parent` (`#root` at the top): a Layer is pinned only in a Stack. */
export function placeError(element: string, parent: string | undefined, where?: string): TypeError | undefined {
  if (element !== 'Layer' || parent === 'Stack') return undefined;
  const under = parent === undefined || parent.startsWith('#') ? 'at the top of the cell' : `in ${article(parent)} ${parent}`;
  return new TypeError(`<Layer> ${under}${at(where)}: a Layer is pinned inside a Stack, <Stack><Image .../><Layer anchor="top-right">...</Layer></Stack>`);
}

const ruleFor = (element: string, name: string): Rule | undefined => ELEMENT_RULES[element as ElementName]?.[name] ?? COMMON_RULES[name];

/**
 * The error for prop `name` set to `value` on `element` when it is not what the element reads (an `opacity` of 2, a `fill` that is a
 * hex colour, a Chart `kind` the app does not draw), naming the prop and what it takes; undefined when it is fine or not checked. A blank
 * (`undefined`, `null`) is always fine: it is the prop not said.
 */
export function propError(element: string, name: string, value: unknown, where?: string): TypeError | undefined {
  if (value === undefined || value === null) return undefined;
  if (name === 'fill' || name === 'stroke') {
    const error = ruleFor(element, name) && inkError(element, name, value, where);
    if (error) return error;
  }
  const wants = ruleFor(element, name)?.(value);
  if (!wants) return undefined;
  return new TypeError(`${written(name, value)} on <${element}> is not ${wants}${at(where)}`);
}

/**
 * A prop as the app reads it: a number said as words (`x="10"`, which is how markup writes it) is the number, and `"true"` or
 * `"false"` for a flag is the flag. Anything else is as given.
 */
export function wireValue(element: string, name: string, value: unknown): unknown {
  const rule = ruleFor(element, name);
  if (typeof value !== 'string' || !rule) return value;
  if (NUMERIC.has(rule) && numeric(value)) return asNumber(value);
  if (rule === flag) return value === 'true';
  return value;
}

/** The data of a series as points, each a number or an `[x, y]` pair; undefined when it is not that. */
function seriesProblem(data: unknown, what: string): string | undefined {
  if (!Array.isArray(data)) return `${what} is ${said(data)}, not a list: numbers or [x, y] pairs`;
  if (data.length > MAX_POINTS) return `${what} has ${data.length} points; a Chart draws at most ${MAX_POINTS} per series (sum or sample them first)`;
  const bad = data.findIndex(p => !(numeric(p) || (Array.isArray(p) && p.length === 2 && p.every(numeric))));
  return bad < 0 ? undefined : `${what}[${bad}] is ${said(data[bad])}: each point is a number or an [x, y] pair`;
}

/**
 * The error for a Chart whose data is not what its kind draws, said from what is known: `props` holds the Chart's other props (its
 * `kind`, its `labels`) when they are. A pie or a donut takes numbers with `labels`, or `{label, value}` slices; a scatter `[x, y]`
 * pairs; the rest numbers or pairs; `series` is at most MAX_SERIES of `{name, data, tone}`. Undefined when it is fine.
 */
export function chartError(name: string, value: unknown, props: Record<string, unknown>, where?: string, whole = false): TypeError | undefined {
  if (value === undefined || value === null) return undefined;
  const named = typeof props.kind === 'string' ? props.kind : undefined;
  // Checked whole, a Chart that says no kind is bars; checked one prop at a time, its kind may yet come.
  const kind = named ?? (whole ? 'bar' : undefined);
  const fail = (problem: string) => new TypeError(`<Chart${named ? ` kind="${named}"` : ''}> ${problem}${at(where)}`);
  if (name === 'data') {
    if (!Array.isArray(value)) return fail(`data is ${said(value)}, not a list`);
    if (value.length > MAX_POINTS) return fail(`data has ${value.length} points; a Chart draws at most ${MAX_POINTS} per series (sum or sample them first)`);
    const slice = (p: unknown) => typeof p === 'object' && p !== null && !Array.isArray(p) && typeof (p as { label?: unknown }).label === 'string' && numeric((p as { value?: unknown }).value);
    if (kind === 'pie' || kind === 'donut') {
      if (value.every(slice)) return undefined;
      if (value.every(numeric)) {
        const labels = props.labels;
        if (labels === undefined) return undefined;
        if (!Array.isArray(labels) || labels.length !== value.length) return fail(`has ${value.length} numbers and ${Array.isArray(labels) ? labels.length : 'no'} labels: one label per number, or data as [{label, value}]`);
        return undefined;
      }
      const bad = value.findIndex(p => !slice(p) && !numeric(p));
      return fail(`data[${bad}] is ${said(value[bad])}: a ${kind}'s data is numbers with labels={[...]}, or [{label: 'a', value: 3}]`);
    }
    if (kind === 'scatter') {
      const bad = value.findIndex(p => !(Array.isArray(p) && p.length === 2 && p.every(numeric)));
      return bad < 0 ? undefined : fail(`data[${bad}] is ${said(value[bad])}: a scatter's data is [x, y] pairs`);
    }
    if (kind === undefined && value.every(p => slice(p) || numeric(p))) return undefined;
    const problem = seriesProblem(value, 'data');
    if (!problem) return undefined;
    return fail(value.some(slice) ? `data is {label, value} slices, which only a pie or a donut takes: say kind="pie", or labels={[...]} and data={numbers}` : problem);
  }
  if (name === 'series') {
    if (!Array.isArray(value)) return fail(`series is ${said(value)}, not a list of {name, data, tone}`);
    if (value.length > MAX_SERIES) return fail(`has ${value.length} series; a Chart draws at most ${MAX_SERIES} (group the rest as other)`);
    for (const [i, s] of value.entries()) {
      if (typeof s !== 'object' || s === null || Array.isArray(s)) return fail(`series[${i}] is ${said(s)}: a series is {name, data, tone}`);
      const stray = Object.keys(s).find(key => !['name', 'data', 'tone'].includes(key));
      if (stray) return fail(`series[${i}] has ${JSON.stringify(stray)}: a series is {name, data, tone}`);
      const problem = seriesProblem((s as { data?: unknown }).data, `series[${i}].data`);
      if (problem) return fail(problem);
      const tone = (s as { tone?: unknown }).tone;
      if (tone !== undefined && !isTone(tone)) return fail(`series[${i}].tone is ${said(tone)}, not a tone: one of ${TONES.join(', ')}`);
    }
  }
  return undefined;
}

/** A Chart checked whole, once all its props are known (tree.ts): its data against its kind and its labels, and its series. */
export function wholeChartError(props: Record<string, unknown>, where?: string): TypeError | undefined {
  if (props.kind === 'stacked' && !Array.isArray(props.series)) return new TypeError(`<Chart kind="stacked"> has no series${at(where)}: stacked bars are series={[{name, data}, ...]}`);
  if ((props.kind === 'pie' || props.kind === 'donut') && Array.isArray(props.data) && props.data.every(numeric) && props.labels === undefined && props.data.length > 0) {
    return new TypeError(`<Chart kind="${props.kind}"> has numbers and no labels${at(where)}: say labels={[...]} (one per number), or data as [{label, value}]`);
  }
  return chartError('data', props.data, props, where, true) ?? chartError('series', props.series, props, where, true);
}

/**
 * Every check of prop `name` set to `value` on the element `element`: that the element reads it, then what it must be. `props` is
 * what the element already has, for the checks that read two props together (a Table's columns and rows, a Chart's kind and data).
 */
export function checkProp(element: ElementName, name: string, value: unknown, where?: string, props: Record<string, unknown> = {}): TypeError | undefined {
  if (!PROPS[element].includes(name)) return unknownPropError(element, name, value, where);
  if (name === 'tone' && element !== 'Button') return toneError(element, value, where);
  if (element === 'Table' && (name === 'columns' || name === 'rows')) return tableError(name === 'columns' ? value : props.columns, name === 'rows' ? value : props.rows, where);
  if (element === 'Chart' && (name === 'data' || name === 'series')) return chartError(name, value, props, where);
  return propError(element, name, value, where);
}

/**
 * `error` as a cell's error: its stack is its own words and, when known, the cell line and column it is about, never the frames of
 * Svelte or the kernel it was thrown through (the person reads the cell, not the runtime).
 */
export function cellError<E extends Error>(error: E, line?: number, column = 1): E {
  error.stack = `${error.name}: ${error.message}${line ? `\n    at repl-cell.js:${line}:${column}` : ''}`;
  return error;
}

export type Handler = (value: unknown) => unknown;

/** What a node reports to the tree it hangs in. */
export interface Attachment {
  /** Something a viewer could see changed. */
  touch(): void;
  /** `id` now answers with `handler`. */
  bind(id: string, handler: Handler, name: string): void;
  /** `id` answers nothing. */
  unbind(id: string): void;
}

/** The last handler id handed out, across every tree: a tree that replaces another never answers the old one's ids. */
let handlerSeq = 0;

/** The prop the app knows an event by: `onclick={...}` (Svelte's `click` event) is `onClick` on the wire. */
export const wireName = (type: string): string => `on${type.slice(0, 1).toUpperCase()}${type.slice(1)}`;

export type Kind = 'element' | 'text' | 'comment' | 'fragment';

/**
 * One element, a piece of text, or one of the two things Svelte needs that are not drawn: a comment (an anchor it inserts
 * before) and a fragment (a bag of nodes that dissolves into the parent it is inserted in). A function prop is an event
 * listener: it is kept in `fns` and shows in `props` as its handler id (`h1`, `h2`, ...), which stays the same for as long as the
 * node keeps a listener for that event. While a node hangs in a tree its ids answer there, and stop when it leaves.
 */
export class ViewNode {
  parent: ViewNode | undefined;
  readonly children: ViewNode[] = [];
  readonly props: Record<string, unknown> = {};
  /** The handler id of each listener, by the prop it travels as (`onClick`). */
  readonly ids = new Map<string, string>();
  private readonly fns = new Map<string, Handler>();
  tree: Attachment | undefined;
  /** The SVG tag it was written as (`rect`), whose attributes `props` holds as written; draw.ts sends them as the Canvas elements. */
  from: string | undefined;
  /** Where the cell writes it (`line 5:3: <rect ...>`), for the errors of checks that run once the markup has run (compile.ts `AT`). */
  at: string | undefined;

  private constructor(readonly kind: Kind, readonly type: string, public text: string) {}
  static element(type: string): ViewNode { return new ViewNode('element', type, ''); }
  static text(value: string): ViewNode { return new ViewNode('text', '#text', value); }
  static comment(value: string): ViewNode { return new ViewNode('comment', '#comment', value); }
  static fragment(): ViewNode { return new ViewNode('fragment', '#fragment', ''); }

  get isText(): boolean { return this.kind === 'text'; }
  get isElement(): boolean { return this.kind === 'element'; }

  /** Sets a prop; `undefined` removes it. */
  setProp(name: string, value: unknown): void {
    if (value === undefined) delete this.props[name];
    else this.props[name] = value;
    this.tree?.touch();
  }

  /** `handler` as the tree runs it: refused, naming the element, while the node is `disabled` (the app sends nothing then; this holds if it does). */
  private gated(name: string, handler: Handler): Handler {
    return (value) => {
      if (this.props.disabled === true) throw new Error(`<${this.type}> is disabled: its ${name} does not run`);
      return handler(value);
    };
  }

  /** `type` (`click`) now has `handler`: the prop `onClick` is its handler id, the same one for as long as the node listens. */
  listen(type: string, handler: Handler): void {
    const name = wireName(type);
    let id = this.ids.get(name);
    if (!id) this.ids.set(name, id = `h${++handlerSeq}`);
    this.fns.set(name, handler);
    this.props[name] = id;
    this.tree?.bind(id, this.gated(name, handler), name);
    this.tree?.touch();
  }

  /** Stops listening for `type`, when `handler` is still the one that listens. */
  unlisten(type: string, handler: Handler): void {
    const name = wireName(type);
    const id = this.ids.get(name);
    if (!id || this.fns.get(name) !== handler) return;
    this.ids.delete(name);
    this.fns.delete(name);
    delete this.props[name];
    this.tree?.unbind(id);
    this.tree?.touch();
  }

  /** Hangs this node and everything under it in `tree`: its ids answer there from now on. */
  attach(tree: Attachment): void {
    this.tree = tree;
    for (const [name, id] of this.ids) tree.bind(id, this.gated(name, this.fns.get(name)!), name);
    for (const child of this.children) child.attach(tree);
  }

  /** Takes this node and everything under it out of its tree: its ids answer nothing. */
  detach(): void {
    const { tree } = this;
    if (!tree) return;
    this.tree = undefined;
    for (const id of this.ids.values()) tree.unbind(id);
    for (const child of this.children) child.detach();
  }
}

/** The attribute compile.ts gives each element for where the cell writes it (`line 5:3: <rect ...>`); the renderer keeps it off the wire. */
export const AT = '__at';

/** The preloaded global compiled components use: the Svelte runtime (the client build and the renderer, see svelte.ts). */
export const SVELTE = '__repl_svelte';

/** A function Svelte compiled a cell's markup to: `render` mounts it. */
export type CellComponent = ((anchor: unknown, props: Record<string, unknown>) => unknown) & { readonly [COMPONENT]: true };
/** The brand of a component a cell's markup was compiled to (and of the element components), so a returned one can be told from any function. */
export const COMPONENT: unique symbol = Symbol.for('repl.ui.component');

/** True for what a cell can return to have it shown: a component its own markup compiled to. */
export function isElement(value: unknown): value is CellComponent {
  return typeof value === 'function' && Reflect.get(value, COMPONENT) === true;
}
