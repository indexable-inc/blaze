/*
 * The model of a cell's UI: a tree of `ViewNode`s, the plain `{type, props, children}` shape the native app draws once it is
 * sent as JSON (harness-native/src/app/chat/genui.rs). Svelte (svelte.ts) builds and updates the tree through its custom
 * renderer; this file is only the data: the element names, the checks on their props, a node that knows its handlers, and the
 * brand of a component `render` can mount. The props each element reads and what each must be are here too (types.ts has them as
 * types), and every check says the cell line it is about when it is known. Nothing here depends on Svelte. Copied from den's repl/src/ui/element.ts and cut to
 * the elements the harness app draws.
 */
import { MAP_RULES } from './map.ts';
import { fileProblem, mediaProblem } from './media.ts';
import { ID_RULE, aspectOf, shaderWire, texturesProblem, uniformsOf } from './shader.ts';

/** The element names the app draws (genui.rs `ELEMENTS`, one `hn.Gen<Name>` prefab each in scene/widgets/genui.flecs). */
export const ELEMENT_NAMES = [
  'Column', 'Row', 'Text', 'Markdown', 'Code', 'Table', 'Badge', 'Stat', 'Card', 'Divider', 'Spacer',
  // Charts, and drawing: shapes in a Canvas's own coordinates, in the scene's inks (draw.ts maps SVG onto these).
  'Chart', 'Canvas', 'Group', 'Path', 'Rect', 'Circle', 'Line', 'Polyline',
  // Controls: what the person presses, drags, types and picks; each sends its event back by handler id (index.ts `dispatch`).
  'Button', 'Toggle', 'Checkbox', 'Slider', 'NumberField', 'Input', 'TextArea', 'Select', 'Choice', 'Radio', 'ColorPicker',
  // A plate Finder files are dropped on: it sends their absolute paths.
  'DropZone',
  // Layout: equal columns, children over one another (a Layer pinned in a Stack), a flow that wraps, a window that scrolls, two panes.
  'Grid', 'Stack', 'Layer', 'Wrap', 'Scroll', 'Split',
  // Folding and tabs: the cell picks what shows under the tab row with {#if}; an Accordion keeps one Disclosure open.
  'Tabs', 'Accordion', 'Disclosure',
  // Floating: drawn over the transcript, clipped to the pane.
  'Popover', 'Menu', 'Dialog', 'Sheet', 'Toast',
  // Status: a bar, a spinner with quiet words, grey lines where text is on its way.
  'Progress', 'Loading', 'Skeleton',
  // Pictures, chips and files: a file kind or symbol, a picture on disk, a pill, a column pressed as a whole, a file chip that opens,
  // a row of a listing (a chip when it names a path), and another cell of this lane shown inline.
  'Icon', 'Image', 'Chip', 'Tile', 'FileRef', 'File', 'CellRef',
  // The agent as the app shows it: Kip, the voxel mascot, live in a mood or a short run of them.
  'Kip',
  // Moving pictures: a video file or URL with its controls, and the agent's own Slang fragment shader (shader.ts), on the GPU.
  'Video', 'Shader',
  // A page of the harness browser (embedded Chromium), live: a view an agent opened (`view={page.view}`) or a URL the app opens.
  'Browser',
  // Places on a real map that pans and zooms (map.ts): Apple's map, which the app lays over the element's card.
  'Map',
] as const;
export type ElementName = typeof ELEMENT_NAMES[number];

const known = new Set<string>(ELEMENT_NAMES);
export const isKnownElement = (name: string): name is ElementName => known.has(name);

/** Den's elements the harness app does not draw (yet): naming one is this error, never a blank or a ReferenceError. None now. */
export const NOT_DRAWN: ReadonlySet<string> = new Set<string>();
export const notDrawnError = (name: string, where?: string): TypeError =>
  new TypeError(`<${name}> is not drawn by the harness app${where ? ` (${where})` : ''}: the elements are ${ELEMENT_NAMES.join(', ')}`);

/** The shapes, and what holds them: a Canvas draws Groups, shapes and Text, and nothing else does. */
export const SHAPES: ReadonlySet<string> = new Set(['Path', 'Rect', 'Circle', 'Line', 'Polyline']);
export const DRAWING: ReadonlySet<string> = new Set(['Group', ...SHAPES]);

/** What every shape takes besides its geometry: its paint, and a press that sends its `value`. */
const SHAPE_PROPS = ['fill', 'stroke', 'strokeWidth', 'fillOpacity', 'strokeOpacity', 'transform', 'value', 'onclick', 'tip'] as const;

/**
 * The props every element takes besides its own (types.ts `CommonProps`), each sent only when given (`commonWire`): its size and
 * padding, its pose (moved, scaled about its middle, turned, faded; sprung by the app), what is over what, how it comes and goes,
 * how it looks under the pointer, and the pointer, key and focus handlers. An element that reads one as its own keeps its own
 * meaning: an Image's, a Chart's, a Scroll's and a Canvas's width and height, a Rect's and a Canvas Text's x and y (geometry), a
 * Layer's x and y (its offset from its anchor). The drawing's shapes and Groups take no size or padding: their geometry is their size.
 */
const SIZE_PROPS = ['width', 'height', 'minWidth', 'maxWidth', 'minHeight', 'maxHeight', 'grow', 'padX', 'padY'] as const;
const POSE_PROPS = ['x', 'y', 'scale', 'rotate', 'opacity'] as const;
const MOTION_PROPS = ['z', 'flip', 'enter', 'exit', 'hover'] as const;
/** The pointer, key and focus events every element hears, as Svelte writes them (`onpointerdown`); the wire's are `onPointerdown`. */
export const COMMON_EVENTS = ['onpointerdown', 'onpointermove', 'onpointerup', 'ondrag', 'onwheel', 'onhover', 'onkeydown', 'onfocus', 'onblur'] as const;
const PAGE_COMMON: readonly string[] = [...SIZE_PROPS, ...POSE_PROPS, ...MOTION_PROPS, ...COMMON_EVENTS, 'tip'];
const DRAW_COMMON: readonly string[] = [...POSE_PROPS, ...MOTION_PROPS, ...COMMON_EVENTS, 'tip'];

/**
 * The props each element reads of its own (types.ts says what each is), its events as Svelte writes them (`onselect`), and `children`
 * when markup between its tags means something. `PROPS` adds the common ones. A prop in neither is an error naming the element and
 * what it takes: Svelte would pass it on and nothing would read it, so `<Code code={src} />` would draw an empty plate.
 */
export const OWN_PROPS: Record<ElementName, readonly string[]> = {
  Column: ['gap', 'pad', 'tip', 'children'],
  Row: ['gap', 'pad', 'tip', 'children'],
  // anchor, size, fill, value and onclick are a Text's inside a Canvas (or <svg>) only (compile.ts says so at the line); its x and y
  // are its point there, and its pose elsewhere.
  Text: ['text', 'tone', 'bold', 'x', 'y', 'anchor', 'size', 'fill', 'value', 'onclick', 'tip', 'children'],
  Markdown: ['text', 'tip', 'children'],
  Code: ['text', 'lang', 'tip', 'children'],
  Table: ['columns', 'rows', 'query', 'onselect', 'onopen', 'tip'],
  Badge: ['text', 'tone', 'tip', 'children'],
  Stat: ['label', 'value', 'note', 'tone', 'tip'],
  Card: ['title', 'gap', 'pad', 'tip', 'children'],
  Divider: ['tip'],
  Spacer: ['size', 'tip'],
  Chart: ['kind', 'data', 'labels', 'series', 'height', 'unit', 'center', 'caption', 'hole', 'pop', 'sep', 'fillOpacity', 'tone', 'legend', 'onselect', 'tip'],
  Canvas: ['view', 'width', 'height', 'value', 'onclick', 'tip', 'children'],
  Group: ['transform', 'opacity', 'value', 'onclick', 'tip', 'children'],
  Path: ['d', 'fillRule', ...SHAPE_PROPS],
  Rect: ['x', 'y', 'width', 'height', 'radius', ...SHAPE_PROPS],
  Circle: ['cx', 'cy', 'r', 'rx', 'ry', ...SHAPE_PROPS],
  Line: ['x1', 'y1', 'x2', 'y2', ...SHAPE_PROPS],
  Polyline: ['points', 'closed', ...SHAPE_PROPS],
  // The controls take exactly what the app reads of them (`elementWire` says what goes on the wire), and the common props.
  Button: ['label', 'tone', 'value', 'disabled', 'onclick', 'children'],
  // `id` is what a Shader's '@id' uniform names: it reads the control's live value.
  Toggle: ['value', 'label', 'disabled', 'id', 'onchange'],
  Checkbox: ['checked', 'label', 'disabled', 'id', 'onchange'],
  Slider: ['value', 'min', 'max', 'step', 'label', 'disabled', 'id', 'onchange'],
  NumberField: ['value', 'min', 'max', 'step', 'label', 'disabled', 'id', 'onchange'],
  Input: ['value', 'placeholder', 'label', 'disabled', 'onchange', 'onsubmit'],
  TextArea: ['value', 'placeholder', 'lines', 'disabled', 'onchange', 'onsubmit'],
  Select: ['options', 'selected', 'placeholder', 'disabled', 'onchange'],
  Choice: ['options', 'selected', 'disabled', 'onchange'],
  Radio: ['options', 'selected', 'row', 'disabled', 'onchange'],
  ColorPicker: ['options', 'selected', 'label', 'disabled', 'onchange'],
  DropZone: ['label', 'ondrop'],
  // Layout, folding, floating, status and pictures: each takes exactly what the app reads of it (`elementWire` says what goes).
  Grid: ['columns', 'fit', 'gap', 'pad', 'children'],
  Stack: ['align', 'children'],
  Layer: ['anchor', 'x', 'y', 'z', 'value', 'onclick', 'children'],
  Wrap: ['gap', 'children'],
  Scroll: ['height', 'horizontal', 'children'],
  Split: ['ratio', 'vertical', 'onchange', 'children'],
  Tabs: ['tabs', 'selected', 'onchange', 'children'],
  Accordion: ['children'],
  Disclosure: ['title', 'open', 'ontoggle', 'children'],
  Popover: ['placement', 'align', 'trigger', 'open', 'ontoggle', 'children'],
  Menu: ['items', 'context', 'open', 'onselect', 'ontoggle', 'children'],
  Dialog: ['title', 'open', 'dismissable', 'onclose', 'children'],
  Sheet: ['title', 'open', 'dismissable', 'onclose', 'children'],
  Toast: ['text', 'tone', 'duration', 'ondismiss', 'children'],
  Progress: ['value', 'max', 'label'],
  Loading: ['label', 'children'],
  Skeleton: ['lines'],
  Icon: ['name', 'kind'],
  Image: ['src', 'width', 'height', 'fit', 'alt', 'id'],
  Chip: ['label', 'selected', 'value', 'onclick', 'children'],
  Tile: ['gap', 'pad', 'value', 'onclick', 'children'],
  FileRef: ['path', 'line', 'name', 'kind', 'onclick'],
  // A listing's row; given a path it is the FileRef chip of that file (tree.ts), and takes what a FileRef takes.
  File: ['name', 'kind', 'size', 'modified', 'value', 'onclick', 'path', 'line'],
  CellRef: ['task', 'title'],
  Kip: ['mood', 'size'],
  Video: ['src', 'poster', 'autoplay', 'loop', 'muted', 'controls', 'width', 'height', 'aspect', 'fit', 'id', 'paused', 'currentTime', 'ontimeupdate', 'onended', 'onplay', 'onpause'],
  // `sourceLine` is compile.ts's (the cell line the source between the tags starts on), for the lines of a compile error.
  Shader: ['source', 'src', 'width', 'height', 'aspect', 'uniforms', 'textures', 'animate', 'id', 'sourceLine', 'children'],
  // One of the two: a view of the harness browser (cdp.ts Page.view), or an http(s) URL the app opens a view at.
  Browser: ['view', 'src'],
  // Places as pins on Apple's map, fitted to them or looking at center with zoom; `here` is where the person is (map.ts).
  Map: ['markers', 'center', 'zoom', 'here'],
};
/** Props the kernel sets itself, never listed as what an element takes. */
const INTERNAL: ReadonlySet<string> = new Set(['sourceLine']);
/** The elements of a drawing that take no size or padding of the common props: their geometry is their size. */
const DRAWN_ONLY: ReadonlySet<string> = new Set(['Group', 'Path', 'Rect', 'Circle', 'Line', 'Polyline']);
/** The props each element reads: its own, then the common ones it does not read as its own. */
export const PROPS: Record<ElementName, readonly string[]> = { ...OWN_PROPS };
for (const element of ELEMENT_NAMES) {
  const own = OWN_PROPS[element];
  PROPS[element] = [...own, ...(DRAWN_ONLY.has(element) ? DRAW_COMMON : PAGE_COMMON).filter(name => !own.includes(name))];
}
/** The props only a Text inside a Canvas reads (its x and y are its point there, and its pose elsewhere). */
export const CANVAS_TEXT_PROPS: ReadonlySet<string> = new Set(['anchor', 'size', 'fill', 'value', 'onclick']);

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
export const CHART_KINDS = ['bar', 'line', 'pie', 'donut', 'segments', 'area', 'scatter', 'stacked'] as const;
/** The most series a Chart draws, and the most points in one. */
export const MAX_SERIES = 8;
export const MAX_POINTS = 2000;
/** The most options a Select, Choice or Radio offers, the longest label a control or an option shows, and the most lines a TextArea is tall. */
export const MAX_OPTIONS = 200;
export const MAX_LABEL = 200;
export const MAX_LINES = 40;
/**
 * The words a Kip's `mood` takes (harness-native-kip act.rs reads the same), each a mood it plays: `celebrate` is two hops. `then`
 * and `and` join them. It plays at most `MAX_KIP_STEPS`, and stands between `KIP_SIZE` points tall.
 */
export const KIP_MOODS = ['wave', 'happy', 'celebrate', 'thinking', 'working', 'oops', 'sleepy', 'idle'] as const;
const KIP_JOINS = new Set(['then', 'and']);
export const MAX_KIP_STEPS = 6;
export const KIP_SIZE = { min: 20, max: 160, given: 64 } as const;
/** The problem with a Kip's `mood`, or its words as the app reads them (one string, the moods in turn). */
export function kipMoodOf(value: unknown): string | { words: string } {
  const said = `words from ${KIP_MOODS.join(', ')}, or a few in turn: mood="wave then idle" or mood={['thinking', 'celebrate']}`;
  const parts = typeof value === 'string' ? [value] : Array.isArray(value) && value.every(v => typeof v === 'string') ? value as string[] : undefined;
  if (!parts) return said;
  const words = parts.flatMap(p => p.toLowerCase().split(/[^a-z]+/)).filter(w => w !== '' && !KIP_JOINS.has(w));
  const unknown = words.find(w => !(KIP_MOODS as readonly string[]).includes(w));
  if (unknown !== undefined) return `${said} (it does not know "${unknown.slice(0, 32)}")`;
  if (words.length === 0) return said;
  if (words.length > MAX_KIP_STEPS) return `at most ${MAX_KIP_STEPS} moods in turn (it has ${words.length})`;
  return { words: words.join(' ') };
}
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
/** Where a Popover's float lines up with its trigger, and what opens it. */
export const POPOVER_ALIGNS = ['start', 'center'] as const;
export const POPOVER_TRIGGERS = ['click', 'hover'] as const;
/**
 * The file kinds an Icon, a FileRef and a File take as `kind` (harness-native media.rs `KINDS`, each drawn as its glyph in the file
 * palette), and the short ways of saying one that media.rs `kind_named` reads as it.
 */
export const FILE_KINDS = [
  'rust', 'typescript', 'tsx', 'javascript', 'json', 'toml', 'yaml', 'markdown', 'nix', 'shell', 'python', 'go', 'html', 'css', 'image',
  'lock', 'git', 'folder', 'folder_open', 'text',
] as const;
const FILE_KIND_ALIASES = [
  'rs', 'ts', 'js', 'jsx', 'mjs', 'md', 'yml', 'sh', 'bash', 'zsh', 'py', 'package', 'picture', 'photo', 'png', 'jpg', 'svg', 'dir',
  'directory', 'folder-open', 'file', 'generic', 'doc', 'txt',
] as const;
const isFileKind = (v: unknown): boolean => typeof v === 'string' && ([...FILE_KINDS, ...FILE_KIND_ALIASES] as readonly string[]).includes(v.toLowerCase());
/** The most swatches a ColorPicker shows. */
export const MAX_SWATCHES = 24;
/** A colour as a ColorPicker sends it: `#rrggbb`. */
const HEX = /^#[0-9a-fA-F]{6}$/;
/** The transitions the app springs (Svelte's names: `transition:fly`, `in:fade`, ...), and a transition's fields. */
export const TRANSITIONS = ['fly', 'fade', 'scale', 'slide'] as const;
const TRANSITION_KEYS = ['kind', 'x', 'y', 'start', 'opacity', 'axis', 'ms', 'delay'];
/** The attribute compile.ts gives an element for the transition it plays as it goes; the renderer keeps it off the wire (svelte.ts). */
export const OUTRO = '__outro';
/** What `hover` takes: the pose and paint under the pointer. */
const HOVER_KEYS = ['x', 'y', 'scale', 'rotate', 'opacity', 'fill', 'stroke', 'tone'];
/** The most a `scale` is: eight times its size. */
export const MAX_SCALE = 8;

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
 * What a bindable element is: `<Slider bind:value={v}>` is the value the person sets, `<Choice bind:selected={s}>` the one they pick,
 * `<Video bind:currentTime={t} bind:paused={p}>` where its playhead is and whether it plays. Its events write the props back (svelte.ts
 * `hostSource`), then the cell's own handler runs, so a binding follows the person and a plain `value={x}` still shows what is
 * written. The first is the element's binding as an error names it. Copied from den's repl/src/ui/element.ts, cut to the app's elements.
 */
export const BINDABLE: Partial<Record<string, readonly string[]>> = {
  Input: ['value'], Slider: ['value'], Toggle: ['value'], NumberField: ['value'], TextArea: ['value'], Checkbox: ['checked'],
  Choice: ['selected'], Radio: ['selected'], Select: ['selected'], ColorPicker: ['selected'],
  Split: ['ratio'], Tabs: ['selected'], Disclosure: ['open'], Popover: ['open'], Menu: ['open'], Dialog: ['open'], Sheet: ['open'],
  Video: ['currentTime', 'paused'],
};

/**
 * The names a model writes for an element's binding out of HTML's and Svelte's habit (`bind:checked` on a switch, `bind:value` on a
 * select), each taken as the binding itself: compile.ts rewrites `bind:checked` on a Toggle to `bind:value`, and a plain `checked={x}`
 * to `value={x}`, so the cell works as written instead of failing on a name.
 */
export const BIND_ALIAS: Partial<Record<string, Readonly<Record<string, string>>>> = {
  Toggle: { checked: 'value' }, Checkbox: { value: 'checked' }, Select: { value: 'selected' }, Choice: { value: 'selected' },
  Radio: { value: 'selected', group: 'selected' }, Tabs: { value: 'selected' }, ColorPicker: { value: 'selected' },
};

/** An event that writes a binding back (`prop`, the element's first binding when not said), and what it writes when not the event's value. */
export interface WriteBack { event: string; prop?: string; value?: unknown }
/**
 * The events that write a binding back when it is not `change` alone: a fold or a float is opened and shut by `toggle`, a Menu also shuts
 * when a row is picked, and closing a Dialog or a Sheet is `open = false`. Copied from den's repl/src/ui/element.ts (Menu added).
 */
export const BOUND_BY: Partial<Record<string, readonly WriteBack[]>> = {
  Disclosure: [{ event: 'toggle' }], Popover: [{ event: 'toggle' }], Menu: [{ event: 'toggle' }, { event: 'select', value: false }],
  Dialog: [{ event: 'close', value: false }], Sheet: [{ event: 'close', value: false }],
  // A Video's playhead follows its timeupdate; it plays and pauses by the person's press.
  Video: [{ event: 'timeupdate', prop: 'currentTime' }, { event: 'play', prop: 'paused', value: false }, { event: 'pause', prop: 'paused', value: true }],
};

/**
 * The error for `bind:<name>` on the element `element` when it binds nothing, or undefined when it is the element's binding (or
 * `bind:this`); `where` is the cell's line. Svelte passes any other name on as a prop no element reads, and nothing writes it back,
 * so the variable would never follow the person.
 */
export function bindError(element: string, name: string, where?: string): TypeError | undefined {
  const bound = BINDABLE[element];
  const alias = BIND_ALIAS[element]?.[name];
  if (name === 'this' || bound?.includes(name) || (alias !== undefined && bound?.includes(alias))) return undefined;
  if (bound) return new TypeError(`bind:${name} on <${element}> binds nothing${at(where)}: ${bound.length === 1 ? `a ${element}'s binding is bind:${bound[0]}={...}` : `a ${element}'s bindings are ${bound.map(prop => `bind:${prop}={...}`).join(' and ')}`}`);
  const takers = Object.entries(BINDABLE).map(([el, props]) => `${el} (${props!.map(prop => `bind:${prop}`).join(', ')})`).join(', ');
  return new TypeError(`bind:${name} on <${element}> binds nothing${at(where)}: only ${takers} take a binding; pass ${name}={...} and an on-handler instead`);
}

/** The props whose value children text becomes: `<Text>hi</Text>` is `{text: 'hi'}`. */
export const TEXT_PROP: Partial<Record<string, string>> = {
  Text: 'text', Badge: 'text', Code: 'text', Markdown: 'text', Button: 'label', Chip: 'label', Loading: 'label', Toast: 'text',
  Shader: 'source',
};
/** The elements whose children are only their words (`<Chip>Open</Chip>`, a Shader's source): another element between their tags is an error. */
export const WORDS_ONLY: ReadonlySet<string> = new Set(['Button', 'Chip', 'Loading', 'Toast', 'Shader']);

export const article = (word: string): string => /^[AEIOU]/.test(word) ? 'an' : 'a';
const squash = (name: string) => name.toLowerCase().replace(/[-_:]/g, '');

/** What an element takes, as an error says it: `a Code takes text, lang (its text is also its children)`; the common props are not listed. */
export function takes(element: ElementName): string {
  const props = OWN_PROPS[element].filter(name => name !== 'children' && name !== 'tip' && !INTERNAL.has(name));
  const list = props.length ? `takes ${props.join(', ')}` : 'takes only tip';
  const kids = !PROPS[element].includes('children') ? '' : TEXT_PROP[element] ? ` (its ${TEXT_PROP[element]} is also its children)` : ' (and children)';
  return `${article(element)} ${element} ${list}${kids}`;
}

/** The error for a prop `name` that `element` does not read, with the one it may have meant. */
export function unknownPropError(element: ElementName, name: string, value: unknown, where?: string): TypeError {
  const meant = PROPS[element].find(prop => squash(prop) === squash(name) || squash(prop) === squash(name.replace(/^stroke-?/, 'stroke')));
  const hint = name === 'children' ? `${article(element)} ${element} takes no children: ${takes(element)}`
    : `${takes(element)}${meant ? `; did you mean ${meant}?` : ''}; it also takes the common props, ${PROPS[element].filter(prop => !OWN_PROPS[element].includes(prop)).join(', ')}`;
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
/** A Video's source: a file, or an http(s) URL it streams. */
const media: Rule = v => mediaProblem(v);
const id: Rule = ID_RULE;
const aspect: Rule = v => aspectOf(v) !== undefined ? undefined : `a width over a height: "16:9", "16/9" or a number (16 / 9), from 0.01 to 100`;
const uniforms: Rule = v => { const u = uniformsOf(v); return typeof u === 'string' ? u : undefined; };
const textures: Rule = v => texturesProblem(v);
/** A Browser's view: the harness browser's id for it, as a Page's `view` says it (decimal digits). */
export const viewOf = (v: unknown): string | undefined =>
  typeof v === 'string' && /^\d{1,20}$/.test(v.trim()) ? v.trim() : typeof v === 'number' && Number.isSafeInteger(v) && v >= 0 ? String(v) : undefined;
const view: Rule = v => viewOf(v) !== undefined ? undefined : `a view of the harness browser, as a page names it: view={page.view} (from browser.open(url) or withHiddenPage)`;
const webUrl: Rule = v => typeof v === 'string' && /^https?:\/\/\S+$/i.test(v.trim()) ? undefined : `an http(s) URL, src="https://example.com"`;

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

/** A transition as the app springs it: every field said, with Svelte's defaults (svelte.ts `trans` maps Svelte's own params onto it). */
export interface Transition { kind: typeof TRANSITIONS[number]; x: number; y: number; start: number; opacity: number; axis: 'x' | 'y'; ms: number; delay: number }
const TRANSITION_SAID = `{kind, x, y, start, opacity, axis, ms, delay} with kind one of ${TRANSITIONS.join(', ')}: enter={{kind: 'fly', y: 8, ms: 200}}, or in:fly={{ y: 8 }}`;

/** The problem with `value` as an `enter` or `exit`, or the transition with every field filled in. */
export function transitionOf(value: unknown): Transition | string {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return `a transition, ${TRANSITION_SAID}`;
  const t = value as Record<string, unknown>;
  const stray = Object.keys(t).find(key => !TRANSITION_KEYS.includes(key));
  if (stray) return `a transition, which has no ${JSON.stringify(stray)}${stray === 'duration' ? ' (its length is ms)' : ''}: ${TRANSITION_SAID}`;
  if (!(TRANSITIONS as readonly string[]).includes(t.kind as string)) return `a transition whose kind is one of ${TRANSITIONS.join(', ')}, not ${said(t.kind)}`;
  const n = (key: string, otherwise: number, test: (n: number) => boolean, wants: string): number | string => {
    if (t[key] === undefined || t[key] === null) return otherwise;
    return numeric(t[key]) && test(asNumber(t[key])) ? asNumber(t[key]) : `a transition whose ${key} is ${wants}, not ${said(t[key])}`;
  };
  if (t.axis !== undefined && t.axis !== 'x' && t.axis !== 'y') return `a transition whose axis is x or y, not ${said(t.axis)}`;
  const fields = {
    x: n('x', 0, () => true, 'a number of points'), y: n('y', 0, () => true, 'a number of points'),
    start: n('start', 0, v => v >= 0 && v <= MAX_SCALE, `a scale from 0 to ${MAX_SCALE}`), opacity: n('opacity', 0, v => v >= 0 && v <= 1, 'a number from 0 to 1'),
    ms: n('ms', 400, v => v >= 0, 'milliseconds, 0 or more'), delay: n('delay', 0, v => v >= 0, 'milliseconds, 0 or more'),
  };
  const bad = Object.values(fields).find(v => typeof v === 'string');
  if (typeof bad === 'string') return bad;
  const f = fields as Record<keyof typeof fields, number>;
  return { kind: t.kind as Transition['kind'], x: f.x, y: f.y, start: f.start, opacity: f.opacity, axis: t.axis === 'x' ? 'x' : 'y', ms: f.ms, delay: f.delay };
}
const transition: Rule = v => { const t = transitionOf(v); return typeof t === 'string' ? t : undefined; };

/** The problem with `value` as a `hover`, or the hover as the app reads it (numbers as numbers). */
export function hoverOf(value: unknown): Record<string, unknown> | string {
  const wants = `how it looks under the pointer, {scale: 1.05, y: -2, fill: 'accent'} (of ${HOVER_KEYS.join(', ')})`;
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return wants;
  const o = value as Record<string, unknown>;
  const stray = Object.keys(o).find(key => !HOVER_KEYS.includes(key));
  if (stray) return `${wants}, which has no ${JSON.stringify(stray)}`;
  const out: Record<string, unknown> = {};
  for (const [key, v] of Object.entries(o)) {
    if (v === undefined || v === null) continue;
    if (key === 'fill' || key === 'stroke') {
      if (!isInk(v)) return `how it looks under the pointer, whose ${key} is an ink (${INKS_SAID}), not ${said(v)}`;
      out[key] = v;
    } else if (key === 'tone') {
      if (!isTone(v)) return `how it looks under the pointer, whose tone is one of ${TONES.join(', ')}, not ${said(v)}`;
      out[key] = v;
    } else {
      const ok = numeric(v) && (key === 'scale' ? asNumber(v) >= 0 && asNumber(v) <= MAX_SCALE : key === 'opacity' ? asNumber(v) >= 0 && asNumber(v) <= 1 : true);
      if (!ok) return `how it looks under the pointer, whose ${key} is ${key === 'scale' ? `a number from 0 to ${MAX_SCALE}` : key === 'opacity' ? 'a number from 0 to 1' : 'a number'}, not ${said(v)}`;
      out[key] = asNumber(v);
    }
  }
  return out;
}
const hover: Rule = v => { const h = hoverOf(v); return typeof h === 'string' ? h : undefined; };

const fileKind: Rule = v => isFileKind(v) ? undefined : `a file kind, one of ${FILE_KINDS.join(', ')} (or ${FILE_KIND_ALIASES.join(', ')})`;

/** A colour a ColorPicker shows or sends: `#rrggbb`, or a tone (the app's ink of it); hex as lower case. Undefined when it is neither. */
function colorOf(value: unknown): string | undefined {
  if (typeof value === 'string' && HEX.test(value)) return value.toLowerCase();
  return isTone(value) ? value : undefined;
}
const COLOR_SAID = `a colour, "#rrggbb" or a tone (${TONES.join(', ')})`;
const color: Rule = v => colorOf(v) === undefined ? COLOR_SAID : undefined;
/** The problem with a ColorPicker's `options`, or its swatches as the app reads them. */
export function colorsOf(value: unknown): string[] | string {
  if (!Array.isArray(value) || value.length === 0) return `a list of 1 to ${MAX_SWATCHES} colours, each "#rrggbb" or a tone: options={['#ff3b30', '#34c759', 'accent']}`;
  if (value.length > MAX_SWATCHES) return `at most ${MAX_SWATCHES} colours (it has ${value.length}); its last well picks any other`;
  const out: string[] = [];
  for (const [i, v] of value.entries()) {
    const c = colorOf(v);
    if (c === undefined) return `a list of colours whose [${i}] is ${said(v)}, not ${COLOR_SAID}`;
    out.push(c);
  }
  return out;
}
const colors: Rule = v => { const c = colorsOf(v); return typeof c === 'string' ? c : undefined; };

/** A cell of this lane as `task` names it: `'t3'` or `3`. */
export function taskOf(value: unknown): string | undefined {
  if (typeof value === 'number' && Number.isInteger(value) && value >= 1) return `t${value}`;
  if (typeof value === 'string' && /^t?[1-9]\d*$/.test(value.trim())) return value.trim().startsWith('t') ? value.trim() : `t${value.trim()}`;
  return undefined;
}
const task: Rule = v => taskOf(v) === undefined ? `a cell of this lane, as its reply names it: task="t3" or task={3}` : undefined;

/** A File's `modified`: words as given, or a time (a Date, or milliseconds since the epoch as `ls()` gives) as `2026-11-05 14:03`. */
export function modifiedOf(value: unknown): string {
  if (typeof value === 'string') return value;
  const date = value instanceof Date ? value : new Date(asNumber(value));
  const two = (n: number) => String(n).padStart(2, '0');
  return `${date.getFullYear()}-${two(date.getMonth() + 1)}-${two(date.getDate())} ${two(date.getHours())}:${two(date.getMinutes())}`;
}
const modified: Rule = v => typeof v === 'string' || (v instanceof Date && Number.isFinite(v.getTime())) || (typeof v === 'number' && Number.isFinite(v))
  ? undefined : 'a time: words ("2 hours ago"), a Date, or milliseconds since the epoch (as ls() gives)';

/** What each prop must be, on every element; a prop not here (and not in `ELEMENT_RULES`) is not checked beyond being one it reads. */
const COMMON_RULES: Record<string, Rule> = {
  gap: size, pad: size, size, width: size, height: size, minWidth: size, maxWidth: size, minHeight: size, maxHeight: size, grow: flag, padX: size, padY: size,
  x: number, y: number, rotate: number, opacity: share, z: number,
  scale: numeral(n => n >= 0 && n <= MAX_SCALE, `a number from 0 to ${MAX_SCALE} (1 is its own size)`),
  flip: flag, enter: transition, exit: transition, hover, tip: words,
};

const SHAPE_RULES: Record<string, Rule> = {
  fill: ink, stroke: ink, strokeWidth: size, fillOpacity: share, strokeOpacity: share, transform: affine,
};
/** The anchors a Canvas's Text hangs from its point by. */
export const TEXT_ANCHORS = ['start', 'middle', 'end'] as const;

/** What a prop must be on one element. */
const ELEMENT_RULES: Partial<Record<ElementName, Record<string, Rule>>> = {
  Text: { bold: flag, x: number, y: number, anchor: oneOf(TEXT_ANCHORS), fill: ink },
  Table: { query: words },
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
  Toggle: { value: flag, label, disabled: flag, id },
  Checkbox: { checked: flag, label, disabled: flag, id },
  Slider: { value: number, min: number, max: number, step: positive, label, disabled: flag, id },
  NumberField: { value: number, min: number, max: number, step: positive, label, disabled: flag, id },
  Input: { value: text, placeholder: label, label, disabled: flag },
  TextArea: { value: text, placeholder: label, lines, disabled: flag },
  Select: { options, placeholder: label, disabled: flag },
  Choice: { options, disabled: flag },
  Radio: { options, row: flag, disabled: flag },
  ColorPicker: { options: colors, selected: color, label, disabled: flag },
  DropZone: { label },
  Grid: { columns: whole(1, MAX_COLUMNS, 'columns'), fit: numeral(n => n > 0, 'a number of points above 0 (the narrowest a column may be)') },
  Stack: { align: oneOf(LAYER_ANCHORS) },
  Layer: { anchor: oneOf(LAYER_ANCHORS), x: number, y: number, z: number },
  Scroll: { height: numeral(n => n > 0 && n <= MAX_SCROLL, `a number of points above 0, at most ${MAX_SCROLL}`), horizontal: flag },
  Split: { ratio: numeral(n => n >= 0.05 && n <= 0.95, 'a share of the room from 0.05 to 0.95'), vertical: flag },
  Tabs: { tabs },
  Disclosure: { title: label, open: flag },
  Popover: { placement: oneOf(PLACEMENTS), align: oneOf(POPOVER_ALIGNS), trigger: oneOf(POPOVER_TRIGGERS), open: flag },
  Menu: { items, context: flag, open: flag },
  Dialog: { title: label, open: flag, dismissable: flag },
  Sheet: { title: label, open: flag, dismissable: flag },
  Toast: { text, duration: numeral(n => n >= 0, 'milliseconds, 0 or more (0 stays until it is pressed)') },
  Progress: { value: number, max: positive, label },
  Loading: { label },
  Skeleton: { lines: whole(1, MAX_SKELETON, 'lines') },
  Icon: { name: label, kind: fileKind },
  Image: { src: file, width: positive, height: positive, fit: oneOf(IMAGE_FITS), alt: label, id },
  Chip: { label, selected: flag },
  FileRef: { path: file, line: whole(1, Number.MAX_SAFE_INTEGER, 'a line'), name: label, kind: fileKind },
  File: {
    name: label, kind: fileKind, size: numeral(n => Number.isInteger(n) && n >= 0, 'a whole number of bytes, 0 or more'), modified,
    path: file, line: whole(1, Number.MAX_SAFE_INTEGER, 'a line'),
  },
  CellRef: { task, title: label },
  Kip: {
    mood: v => { const m = kipMoodOf(v); return typeof m === 'string' ? m : undefined; },
    size: numeral(n => n >= KIP_SIZE.min && n <= KIP_SIZE.max, `points from ${KIP_SIZE.min} to ${KIP_SIZE.max} (how tall Kip stands)`),
  },
  Video: {
    src: media, poster: file, autoplay: flag, loop: flag, muted: flag, controls: flag, width: positive, height: positive, aspect,
    fit: oneOf(IMAGE_FITS), id, paused: flag, currentTime: numeral(n => n >= 0, 'seconds, 0 or more'),
  },
  Shader: {
    source: v => typeof v === 'string' ? undefined : 'Slang source (a string): float4 main(ShaderInput in) { ... }', src: file,
    width: positive, height: positive, aspect, uniforms, textures, animate: flag, id, sourceLine: whole(1, Number.MAX_SAFE_INTEGER, 'a line'),
  },
  Browser: { view, src: webUrl },
  Map: MAP_RULES,
};

/** The elements that go to the app as exactly the props `elementWire` gives them: the controls, and every element of tier 3. */
export const WIRED: ReadonlySet<string> = new Set([
  'Button', 'Toggle', 'Checkbox', 'Slider', 'NumberField', 'Input', 'TextArea', 'Select', 'Choice', 'Radio', 'ColorPicker', 'DropZone',
  'Grid', 'Stack', 'Layer', 'Wrap', 'Scroll', 'Split', 'Tabs', 'Accordion', 'Disclosure', 'Popover', 'Menu', 'Dialog', 'Sheet', 'Toast',
  'Progress', 'Loading', 'Skeleton', 'Icon', 'Image', 'Chip', 'Tile', 'FileRef', 'File', 'CellRef', 'Kip', 'Video', 'Shader', 'Browser',
]);

/** Two values the same as JSON says them (a tab's value against the selected one). */
const same = (a: unknown, b: unknown): boolean => a === b || JSON.stringify(a) === JSON.stringify(b);

/**
 * An element's props as the app reads them (the harness app's genui.rs is written from this exact shape), once all are known:
 * defaults filled in, words that are numbers made numbers, options, tabs and rows as objects, an Image's or a FileRef's file as an
 * absolute path (tree.ts resolves it), and its handler ids as `onClick`, `onChange`, `onSubmit`, `onToggle`, `onSelect`, `onClose`,
 * `onDismiss` and `onDrop`. The common props are not here: `commonWire` adds them after. Throws the error naming what is wrong (a Button with no label, a Slider whose min is not below its max, a Grid with both
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
      // What a press sends; the app sends null when there is none.
      if (props.value !== undefined && props.value !== null) out.value = props.value;
      break;
    }
    case 'Toggle':
      out.value = props.value === true;
      put('label', 'disabled', 'id', 'onChange');
      break;
    case 'Checkbox':
      out.checked = props.checked === true;
      put('label', 'disabled', 'id', 'onChange');
      break;
    case 'Slider': {
      const min = num('min') ?? 0;
      const max = num('max') ?? 100;
      if (!(min < max)) throw fail(`has min=${min} and max=${max}`, `a Slider's min is below its max (0 and 100 when not said)`);
      out.value = num('value') ?? min;
      out.min = min;
      out.max = max;
      if (given('step')) out.step = num('step');
      put('label', 'disabled', 'id', 'onChange');
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
      put('label', 'disabled', 'id', 'onChange');
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
    case 'ColorPicker': {
      // The tone palette when it says no colours; the last well (the system colour panel) picks any other.
      const list = given('options') ? colorsOf(props.options) : [...TONES];
      if (typeof list === 'string') throw fail(`options are not ${list}`);
      out.options = list;
      out.selected = given('selected') ? colorOf(props.selected) ?? null : null;
      put('label', 'disabled', 'onChange');
      break;
    }
    case 'DropZone':
      put('label', 'onDrop');
      break;
    case 'Grid':
      if (given('columns') && given('fit')) throw fail('has both columns and fit', 'a Grid has columns={3} (that many equal columns) or fit={160} (as many as fit, each at least that wide), not both');
      if (given('fit')) out.fit = num('fit');
      else out.columns = num('columns') ?? 2;
      out.gap = num('gap') ?? 12;
      out.pad = num('pad') ?? 0;
      break;
    case 'Stack':
      put('align');
      break;
    case 'Accordion':
      break;
    case 'Layer':
      out.anchor = given('anchor') ? props.anchor : 'top-left';
      out.x = num('x') ?? 0;
      out.y = num('y') ?? 0;
      out.z = num('z') ?? 0;
      if (props.value !== undefined && props.value !== null) out.value = props.value;
      put('onClick');
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
      out.align = given('align') ? props.align : 'start';
      out.trigger = given('trigger') ? props.trigger : 'click';
      out.open = yes('open', false);
      put('onToggle');
      break;
    case 'Menu': {
      if (!given('items')) throw fail('has no items', `items={['Rename', '-', {label: 'Delete', value: 'rm', danger: true}]}`);
      const list = itemsOf(props.items);
      if (typeof list === 'string') throw fail(`items are not ${list}`);
      out.items = list;
      out.context = yes('context', false);
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
      if (!given('name') && !given('kind')) throw fail('has no name or kind', `say kind="rust" (a file kind, drawn as its glyph) or name="..." (a symbol's name or a file kind)`);
      if (given('name')) out.name = String(props.name);
      put('kind');
      break;
    case 'Image':
      if (!given('src')) throw fail('has no src', 'src="/abs/path/picture.png" (a file on disk; a relative path is from the working directory)');
      out.src = props.src;
      put('width', 'height');
      out.fit = given('fit') ? props.fit : 'contain';
      if (given('alt')) out.alt = String(props.alt);
      put('id');
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
      if (given('name')) out.name = String(props.name);
      put('kind', 'onClick');
      break;
    case 'File': {
      // A File given a path went as a FileRef (tree.ts); this is a row of a listing. A trailing / names a folder.
      const name = needs('name', 'say name="notes.md" (a trailing / is a folder, name="src/"), or path="/abs/file" for a chip that opens it');
      if (name.length > MAX_LABEL) throw fail(`has a name of ${name.length} characters; a File's is at most ${MAX_LABEL}`);
      out.name = name;
      put('kind');
      if (given('size')) out.size = num('size');
      if (given('modified')) out.modified = modifiedOf(props.modified);
      if (props.value !== undefined && props.value !== null) out.value = props.value;
      put('onClick');
      break;
    }
    case 'CellRef':
      // tree.ts resolved the task to the tool call that ran it, or refused it.
      out.task = props.task;
      out.call = props.call;
      if (given('title')) out.title = String(props.title);
      break;
    case 'Kip': {
      // Waves when no mood is said; the rules checked the words, so this reads them once more as the app does.
      const mood = given('mood') ? kipMoodOf(props.mood) : { words: 'wave' };
      if (typeof mood === 'string') throw fail(`has mood ${JSON.stringify(props.mood).slice(0, 60)}`, mood);
      out.mood = mood.words;
      out.size = num('size') ?? KIP_SIZE.given;
      break;
    }
    case 'Video':
      // src and poster are absolute paths (or src an http(s) URL) by now: tree.ts resolves them, and they must be there.
      if (!given('src')) throw fail('has no src', 'src="~/Movies/clip.mp4" (a file on disk, relative ones from the working directory) or an http(s) URL');
      out.src = props.src;
      put('poster');
      out.autoplay = yes('autoplay', false);
      out.loop = yes('loop', false);
      out.muted = yes('muted', true);
      out.controls = yes('controls', true);
      put('width', 'height');
      if (given('aspect')) out.aspect = aspectOf(props.aspect);
      out.fit = given('fit') ? props.fit : 'contain';
      put('id');
      if (given('paused')) out.paused = props.paused === true;
      if (given('currentTime')) out.currentTime = num('currentTime');
      put('onTimeUpdate', 'onEnded', 'onPlay', 'onPause');
      break;
    case 'Shader':
      return shaderWire(props, where);
    case 'Browser': {
      // A view (live, drawn from the host's frames) or a URL (the app opens a view of its own at it): exactly one.
      if (given('view') === given('src')) {
        throw fail(given('view') ? 'has both a view and a src' : 'has no view and no src',
          'view={page.view} draws a page an agent opened (browser.open(url)), src="https://…" opens one at that URL');
      }
      if (given('view')) {
        const v = viewOf(props.view);
        if (v === undefined) throw fail(`has view ${JSON.stringify(props.view).slice(0, 40)}`, 'view={page.view}, the id a harness browser page gives');
        out.view = v;
      } else {
        out.src = String(props.src).trim();
      }
      break;
    }
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

/** The events of more than one word, as the wire spells them (camel case: `ontimeupdate` is `onTimeUpdate`). */
const EVENT_WORDS: Readonly<Record<string, string>> = { timeupdate: 'TimeUpdate' };

/** The common props an element takes and does not read as its own, by their names on the wire (`onPointerdown` for `onpointerdown`). */
const COMMON_WIRE = new Map<string, Array<[string, string]>>(ELEMENT_NAMES.map(element => [element,
  PROPS[element].filter(name => !OWN_PROPS[element].includes(name) && PAGE_COMMON.includes(name))
    .map(name => [name, /^on/.test(name) ? wireName(name.slice(2)) : name] as [string, string])]));

/**
 * The common props of an element (`PAGE_COMMON`) as the app reads them, put in `out` (its wire so far) when given: numbers as numbers,
 * flags as flags, an `enter` or `exit` with every field filled in (`transitionOf`), a `hover` checked, and each handler as its id. Run
 * on every element after `elementWire` or draw.ts `drawWire`, which leave them out or pass them as given. `props` are the element's as
 * plain JSON; an error names `where`.
 */
export function commonWire(element: string, props: Record<string, unknown>, out: Record<string, unknown>, where?: string): Record<string, unknown> {
  for (const [name, wire] of COMMON_WIRE.get(element) ?? []) {
    const value = props[wire];
    if (value === undefined || value === null) continue;
    if (name === 'enter' || name === 'exit') {
      const t = transitionOf(value);
      if (typeof t === 'string') throw new TypeError(`${written(name, value)} on <${element}> is not ${t}${at(where)}`);
      out[wire] = t;
    } else if (name === 'hover') {
      const h = hoverOf(value);
      if (typeof h === 'string') throw new TypeError(`${written(name, value)} on <${element}> is not ${h}${at(where)}`);
      out[wire] = h;
    } else out[wire] = wireValue(element, name, value);
  }
  return out;
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
    if (kind === 'pie' || kind === 'donut' || kind === 'segments') {
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
export function wireName(type: string): string { return `on${EVENT_WORDS[type] ?? `${type.slice(0, 1).toUpperCase()}${type.slice(1)}`}`; }

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
