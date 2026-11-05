/*
 * The model of a cell's UI: a tree of `ViewNode`s, the plain `{type, props, children}` shape the native app draws once it is
 * sent as JSON (harness-native/src/app/chat/genui.rs). Svelte (svelte.ts) builds and updates the tree through its custom
 * renderer; this file is only the data: the element names, the checks on their props, a node that knows its handlers, and the
 * brand of a component `render` can mount. The props each element reads and what each must be are here too (types.ts has them as
 * types), and every check says the cell line it is about when it is known. Nothing here depends on Svelte. Copied from den's repl/src/ui/element.ts and cut to
 * the elements the harness app draws.
 */

/** The element names the app draws (genui.rs `ELEMENTS`, one `hn.Gen<Name>` prefab each in scene/widgets/genui.flecs). */
export const ELEMENT_NAMES = [
  'Column', 'Row', 'Text', 'Markdown', 'Code', 'Table', 'Badge', 'Stat', 'Card', 'Divider', 'Spacer',
  // Charts, and drawing: shapes in a Canvas's own coordinates, in the scene's inks (draw.ts maps SVG onto these).
  'Chart', 'Canvas', 'Group', 'Path', 'Rect', 'Circle', 'Line', 'Polyline',
] as const;
export type ElementName = typeof ELEMENT_NAMES[number];

const known = new Set<string>(ELEMENT_NAMES);
export const isKnownElement = (name: string): name is ElementName => known.has(name);

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
};
/** The props only a Text inside a Canvas reads. */
export const CANVAS_TEXT_PROPS: ReadonlySet<string> = new Set(['x', 'y', 'anchor', 'size', 'fill']);

export type Tone = 'ink' | 'muted' | 'strong' | 'error' | 'accent' | 'success' | 'warning';
/** Every tone, as a record so the type and this list cannot drift apart. */
const TONE_SET: Record<Tone, true> = { ink: true, muted: true, strong: true, error: true, accent: true, success: true, warning: true };
export const TONES = Object.keys(TONE_SET) as Tone[];
export const isTone = (v: unknown): v is Tone => typeof v === 'string' && Object.hasOwn(TONE_SET, v);
/** The elements that take a `tone`. */
export const TONED: ReadonlySet<string> = new Set(['Text', 'Badge', 'Stat', 'Chart']);

/** The inks a shape's `fill` and `stroke` take (types.ts `Ink`): a tone, the quiet `faint`, a chart's series inks, or `none`. */
export const INKS: readonly string[] = [...TONES, 'faint', ...Array.from({ length: 8 }, (_, i) => `series${i + 1}`), 'none'];
export const isInk = (v: unknown): v is string => typeof v === 'string' && INKS.includes(v);
/** The inks as an error lists them. */
const INKS_SAID = `${TONES.join(', ')}, faint, series1..series8, none (series1.. for categorical data)`;
export const CHART_KINDS = ['bar', 'line', 'pie', 'donut', 'area', 'scatter', 'stacked'] as const;
/** The most series a Chart draws, and the most points in one. */
export const MAX_SERIES = 8;
export const MAX_POINTS = 2000;

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

/** The error for `bind:<name>` on one of the app's elements: none takes a binding yet (there are no inputs). */
export function bindError(element: string, name: string, where?: string): TypeError | undefined {
  if (name === 'this') return undefined;
  return new TypeError(`bind:${name} on <${element}> binds nothing${at(where)}: the harness app's elements show values and take no input yet`);
}

/** The props whose value children text becomes: `<Text>hi</Text>` is `{text: 'hi'}`. */
export const TEXT_PROP: Partial<Record<string, string>> = { Text: 'text', Badge: 'text', Code: 'text', Markdown: 'text' };

const article = (word: string) => /^[AEIOU]/.test(word) ? 'an' : 'a';
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
const oneOf = (list: readonly string[]): Rule => v => typeof v === 'string' && list.includes(v) ? undefined : `one of ${list.join(', ')}`;
const number: Rule = v => numeric(v) ? undefined : 'a number';
const share: Rule = v => numeric(v) && asNumber(v) >= 0 && asNumber(v) <= 1 ? undefined : 'a number from 0 to 1';
const size: Rule = v => numeric(v) && asNumber(v) >= 0 ? undefined : 'a number of points, 0 or more';
const words: Rule = v => typeof v === 'string' || (typeof v === 'number' && Number.isFinite(v)) ? undefined : 'words (a string)';
const flag: Rule = v => typeof v === 'boolean' || v === 'true' || v === 'false' ? undefined : 'true or false';
const ink: Rule = v => isInk(v) || v === 'currentColor' ? undefined : `an ink: ${INKS_SAID}`;
/** A list of numbers, from `"0,0 10,5"`, `[0, 0, 10, 5]` or `[[0, 0], [10, 5]]`; undefined when it is none of those. */
export const numbers = (v: unknown): number[] | undefined => typeof v === 'string' ? v.trim().split(/[\s,]+/).filter(Boolean).map(Number)
  : Array.isArray(v) ? v.flat().map(n => typeof n === 'number' ? n : typeof n === 'string' && numeric(n) ? Number(n) : NaN) : undefined;
const affine: Rule = v => typeof v === 'string' || (Array.isArray(v) && v.length === 6 && v.every(numeric)) ? undefined
  : `an SVG transform, "translate(10 20) rotate(45)", or six numbers [a, b, c, d, e, f]`;

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
};

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
  if ((rule === number || rule === share || rule === size) && numeric(value)) return asNumber(value);
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
  if (name === 'tone') return toneError(element, value, where);
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

  /** `type` (`click`) now has `handler`: the prop `onClick` is its handler id, the same one for as long as the node listens. */
  listen(type: string, handler: Handler): void {
    const name = wireName(type);
    let id = this.ids.get(name);
    if (!id) this.ids.set(name, id = `h${++handlerSeq}`);
    this.fns.set(name, handler);
    this.props[name] = id;
    this.tree?.bind(id, handler, name);
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
    for (const [name, id] of this.ids) tree.bind(id, this.fns.get(name)!, name);
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
