import { ELEMENT_NAMES, SHAPES, checkProp, inkError, isKnownElement, numbers, wireValue } from './element.ts';
import type { ElementName } from './element.ts';

/*
 * Drawing: a Canvas and the Group, shapes and Text in it, and the subset of SVG that models write first (`<svg>`, `<path>`, ...),
 * which the kernel takes as the same elements so the app only ever sees the Canvas ones. An SVG element keeps its attributes as
 * written (`stroke-width`, `transform="rotate(45)"`) while the markup runs, each checked as it is set (`svgError`); `drawWire` turns a
 * node into what the app reads when the tree is sent: names in camelCase, numbers as numbers, a transform as one matrix, paint that a
 * `<g>` or `<svg>` says passed down to the shapes that do not, and every fill and stroke said, so the app needs no defaults.
 */

/** The SVG tags taken, and the element each is sent as. */
export const SVG_TAGS: Readonly<Record<string, ElementName>> = {
  svg: 'Canvas', g: 'Group', path: 'Path', rect: 'Rect', circle: 'Circle', ellipse: 'Circle', line: 'Line', polyline: 'Polyline', polygon: 'Polyline', text: 'Text',
};
export const isSvgTag = (name: string): boolean => Object.hasOwn(SVG_TAGS, name);
/** The tags as an error lists them. */
export const SVG_SAID = 'path, rect, circle, ellipse, line, polyline, polygon, g, text';

/** Paint a `<g>` or `<svg>` says for what it holds, as SVG inherits it. */
const PAINT = ['fill', 'stroke', 'stroke-width', 'fill-opacity', 'stroke-opacity', 'fill-rule', 'stroke-linecap', 'stroke-linejoin', 'font-size', 'font-weight', 'text-anchor'];
/** The events a drawn element hears (its press, the pointer over it), sent as the Canvas element's handler ids (tree.ts). */
const EVENTS = ['onclick', 'onpointerdown', 'onpointermove', 'onpointerup', 'ondrag', 'onwheel', 'onhover'];
const SHAPE_ATTRS = ['fill', 'stroke', 'stroke-width', 'fill-opacity', 'stroke-opacity', 'opacity', 'transform', 'stroke-linecap', 'stroke-linejoin', 'class', 'id', 'tip', ...EVENTS];
/** The attributes each SVG tag takes. */
const SVG_ATTRS: Readonly<Record<string, readonly string[]>> = {
  svg: ['viewBox', 'width', 'height', 'xmlns', 'xmlns:xlink', 'version', 'preserveAspectRatio', 'class', 'id', 'tip', ...EVENTS, ...PAINT],
  g: ['transform', 'opacity', 'class', 'id', 'tip', ...EVENTS, ...PAINT],
  path: ['d', 'fill-rule', ...SHAPE_ATTRS],
  rect: ['x', 'y', 'width', 'height', 'rx', 'ry', ...SHAPE_ATTRS],
  circle: ['cx', 'cy', 'r', ...SHAPE_ATTRS],
  ellipse: ['cx', 'cy', 'rx', 'ry', ...SHAPE_ATTRS],
  line: ['x1', 'y1', 'x2', 'y2', ...SHAPE_ATTRS],
  polyline: ['points', ...SHAPE_ATTRS],
  polygon: ['points', ...SHAPE_ATTRS],
  text: ['x', 'y', 'text-anchor', 'font-size', 'font-weight', 'fill', 'class', 'id', 'tip', ...EVENTS],
};
/** Attributes models reach for that the scene has no way to honour, and what to say instead. */
const REFUSED: Readonly<Record<string, string>> = {
  style: 'no styles: say fill= and stroke= with an ink',
  'dominant-baseline': 'a text\'s y is its baseline; move y instead',
  'font-family': 'there is one font',
  'stroke-dasharray': 'strokes are solid',
};

const at = (where?: string) => where ? ` (${where})` : '';
const said = (name: string, value: unknown) => value === undefined ? name : typeof value === 'string' ? `${name}=${JSON.stringify(value)}` : `${name}={${JSON.stringify(value) ?? String(value)}}`;
const isNumeric = (v: unknown) => (typeof v === 'number' && Number.isFinite(v)) || (typeof v === 'string' && /^\s*[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?\s*$/.test(v));

/** A length as a number of points: `120`, `"120"`, `"120px"`; `'%'` for a share of the room; undefined when it is neither. */
function length(v: unknown): number | '%' | undefined {
  if (isNumeric(v)) return Number(v);
  if (typeof v !== 'string') return undefined;
  if (/^\s*[+-]?(\d+\.?\d*|\.\d+)px\s*$/.test(v)) return Number(v.trim().slice(0, -2));
  if (/^\s*(\d+\.?\d*|\.\d+)%\s*$/.test(v)) return '%';
  return undefined;
}

type Affine = [number, number, number, number, number, number];
const IDENTITY: Affine = [1, 0, 0, 1, 0, 0];
const times = (m: Affine, n: Affine): Affine => [
  m[0] * n[0] + m[2] * n[1], m[1] * n[0] + m[3] * n[1],
  m[0] * n[2] + m[2] * n[3], m[1] * n[2] + m[3] * n[3],
  m[0] * n[4] + m[2] * n[5] + m[4], m[1] * n[4] + m[3] * n[5] + m[5],
];
const radians = (degrees: number) => degrees * Math.PI / 180;
/** A matrix entry without float noise (`cos(90°)` is 6e-17) or a negative zero, so the same transform is always the same bytes. */
const clean = (v: number) => { const r = Math.round(v * 1e9) / 1e9; return r === 0 ? 0 : r; };

/**
 * An SVG transform list (`translate(100 100) rotate(-90)`: matrix, translate, scale, rotate with an optional centre, skewX, skewY)
 * as one matrix, `[a, b, c, d, e, f]` in SVG's order; or six numbers as given. A string the grammar does not read is the error's words.
 */
export function parseTransform(value: unknown): Affine | string {
  if (Array.isArray(value)) {
    return value.length === 6 && value.every(isNumeric) ? value.map(v => clean(Number(v))) as Affine : 'six numbers [a, b, c, d, e, f]';
  }
  if (typeof value !== 'string') return 'an SVG transform, "translate(10 20) rotate(45)"';
  let m = IDENTITY;
  const text = value.trim();
  const step = /\s*(matrix|translate|scale|rotate|skewX|skewY)\s*\(([^)]*)\)\s*,?/y;
  let index = 0;
  while (index < text.length) {
    step.lastIndex = index;
    const found = step.exec(text);
    if (!found) return `an SVG transform: "${text.slice(index, index + 24)}" is not matrix(), translate(), scale(), rotate(), skewX() or skewY()`;
    index = step.lastIndex;
    const name = found[1]!;
    const args = found[2]!.trim() ? found[2]!.trim().split(/[\s,]+/).map(Number) : [];
    if (args.some(n => !Number.isFinite(n))) return `an SVG transform whose ${name}() takes numbers, not "${found[2]!.trim()}"`;
    const arity: Record<string, number[]> = { matrix: [6], translate: [1, 2], scale: [1, 2], rotate: [1, 3], skewX: [1], skewY: [1] };
    if (!arity[name]!.includes(args.length)) return `an SVG transform whose ${name}() takes ${arity[name]!.join(' or ')} numbers, not ${args.length}`;
    const [p = 0, q, r] = args;
    let n: Affine;
    switch (name) {
      case 'matrix': n = args as Affine; break;
      case 'translate': n = [1, 0, 0, 1, p, q ?? 0]; break;
      case 'scale': n = [p, 0, 0, q ?? p, 0, 0]; break;
      case 'rotate': {
        const [cos, sin] = [Math.cos(radians(p)), Math.sin(radians(p))];
        const turn: Affine = [cos, sin, -sin, cos, 0, 0];
        n = q === undefined ? turn : times(times([1, 0, 0, 1, q, r!], turn), [1, 0, 0, 1, -q, -r!]);
        break;
      }
      case 'skewX': n = [1, 0, Math.tan(radians(p)), 1, 0, 0]; break;
      default: n = [1, Math.tan(radians(p)), 0, 1, 0, 0];
    }
    m = times(m, n);
  }
  return m.map(clean) as Affine;
}

/** What an SVG tag takes, as an error says it. */
const svgTakes = (tag: string) => `a <${tag}> takes ${SVG_ATTRS[tag]!.filter(name => !['class', 'id', 'xmlns', 'xmlns:xlink', 'version'].includes(name) && !EVENTS.includes(name)).join(', ')}, and hears ${EVENTS.join(', ')}`;

/**
 * The error for attribute `name` set to `value` on the SVG tag `tag` (`rect`), or undefined when the kernel can send it: an attribute
 * the tag has not, a colour that is not an ink, a length that is not one, a line cap the scene does not draw.
 */
export function svgError(tag: string, name: string, value: unknown, where?: string): TypeError | undefined {
  const fail = (wants: string) => new TypeError(`${said(name, value)} on <${tag}> ${wants}${at(where)}`);
  if (Object.hasOwn(REFUSED, name)) return fail(`is not drawn: ${REFUSED[name]}`);
  if (!SVG_ATTRS[tag]!.includes(name)) {
    if (/^on/.test(name)) return new TypeError(`${name} on <${tag}> answers nothing${at(where)}: a drawing hears ${EVENTS.join(', ')}; for keys and transitions draw the Canvas elements (<Rect>, <Group>, ...)`);
    if (tag === 'text' && name === 'transform') return new TypeError(`<text transform> is not drawn${at(where)}: a <text> takes x and y; put it in a <g transform="...">`);
    if (tag === 'svg' && name === 'opacity') return new TypeError(`<svg opacity> is not drawn${at(where)}: put what fades in a <g opacity="...">`);
    return new TypeError(`<${tag}> has no attribute ${JSON.stringify(name)}${at(where)}: ${svgTakes(tag)}`);
  }
  if (value === undefined || value === null || value === false || EVENTS.includes(name)) return undefined;
  switch (name) {
    case 'fill': case 'stroke': return inkError(tag, name, value, where);
    case 'class': case 'id': case 'xmlns': case 'xmlns:xlink': case 'version': return undefined;
    case 'tip': return typeof value === 'string' ? undefined : fail('is not words (a string)');
    case 'preserveAspectRatio': return value === 'xMidYMid meet' || value === 'xMidYMid' ? undefined : fail('is not drawn: a drawing is always fitted in the middle of its box, whole (xMidYMid meet)');
    case 'stroke-linecap': case 'stroke-linejoin': return value === 'round' ? undefined : fail('is not drawn: lines are drawn round, so say round or leave it out');
    case 'fill-rule': return value === 'nonzero' || value === 'evenodd' ? undefined : fail('is not one of nonzero, evenodd');
    case 'text-anchor': return value === 'start' || value === 'middle' || value === 'end' ? undefined : fail('is not one of start, middle, end');
    case 'font-weight': return value === 'bold' || value === 'normal' || isNumeric(value) ? undefined : fail('is not bold, normal or a weight (600 and up is bold)');
    case 'transform': { const m = parseTransform(value); return typeof m === 'string' ? fail(`is not ${m}`) : undefined; }
    case 'viewBox': {
      const n = numbers(value);
      return n && n.length === 4 && n.every(Number.isFinite) && n[2]! > 0 && n[3]! > 0 ? undefined : fail('is not four numbers, minX minY width height: "0 0 100 50"');
    }
    case 'points': { const n = numbers(value); return n && n.length % 2 === 0 && n.every(Number.isFinite) ? undefined : fail('is not x,y pairs: "0,0 10,5 20,0"'); }
    case 'd': return typeof value === 'string' && /^[MmLlHhVvCcSsQqTtAaZz\d\s.,+eE-]*$/.test(value) ? undefined : fail('is not SVG path data, "M 0 0 L 10 10 Z" (M L H V C S Q T A Z)');
    case 'opacity': case 'fill-opacity': case 'stroke-opacity': return isNumeric(value) && Number(value) >= 0 && Number(value) <= 1 ? undefined : fail('is not a number from 0 to 1');
    case 'width': case 'height': {
      const n = length(value);
      if (n === '%') return tag === 'svg' ? undefined : fail('is not a number: a share of the room is for an <svg>\'s own width');
      return n !== undefined && n >= 0 ? undefined : fail('is not a length: a number of points, "120" or "120px" (or "100%" on an <svg>)');
    }
    case 'font-size': case 'stroke-width': case 'r': case 'rx': case 'ry': {
      const n = length(value);
      return typeof n === 'number' && n >= 0 ? undefined : fail('is not a number of points, 0 or more');
    }
    default: return isNumeric(value) || (typeof length(value) === 'number') ? undefined : fail('is not a number');
  }
}

/** Every check of an attribute as it is set, on an element of the app's or an SVG tag. */
export function attributeError(element: string, name: string, value: unknown, where?: string, props: Record<string, unknown> = {}): TypeError | undefined {
  if (isSvgTag(element)) return svgError(element, name, value, where);
  if (isKnownElement(element)) return checkProp(element, name, value, where, props);
  return undefined;
}

/** What an element that is no element of the app's should be, said for the common HTML ones. */
const INSTEAD: Readonly<Record<string, string>> = {
  div: 'Column (stacked) or Row (side by side), Card for a plate', section: 'Column or Card', main: 'Column', article: 'Column or Card',
  header: 'Row or Column', footer: 'Row or Column', nav: 'Row', aside: 'Column', span: 'Text', p: 'Text', label: 'Text', strong: 'Text bold',
  b: 'Text bold', em: 'Text', i: 'Text', small: 'Text tone="muted"', h1: 'Text bold', h2: 'Text bold', h3: 'Text bold', h4: 'Text bold',
  h5: 'Text bold', h6: 'Text bold', hr: 'Divider', br: 'a new Text', pre: 'Code', code: 'Code', table: 'Table', tr: 'Table rows={...}',
  td: 'Table rows={...}', th: 'Table columns={...}', ul: 'Column of Text', ol: 'Column of Text', li: 'Text', img: 'Image src="/path/to/picture.png"',
  details: 'Disclosure title="..."', summary: 'Disclosure title="..."', dialog: 'Dialog', progress: 'Progress', a: 'FileRef path="..." (a file) or Button',
  video: 'Video src="/path/to/clip.mp4"', canvas: 'Canvas (shapes) or Shader (a Slang fragment shader)',
  button: 'Button', input: 'Input (words), Slider or NumberField (a number), Checkbox or Toggle (on or off)', textarea: 'TextArea', select: 'Select', form: 'Column', foreignObject: 'no HTML inside a drawing: words in an <svg> are <text x y>',
  tspan: 'one <text> per run of words', defs: 'nothing: no gradients or patterns, colour is an ink', linearGradient: 'an ink: no gradients',
  radialGradient: 'an ink: no gradients', clipPath: 'nothing yet: no clipping', mask: 'nothing yet: no masks', use: 'the shape written again',
  title: 'tip="..." on the shape', image: 'an Image beside the drawing (no pictures inside one)', marker: 'a small <path> at the line\'s end',
};

/** The error for a lowercase element that is neither one of the app's nor an SVG tag the kernel takes. */
export function unknownTagError(tag: string, where?: string): TypeError {
  const instead = INSTEAD[tag] ? `; for <${tag}> use ${INSTEAD[tag]}` : '';
  return new TypeError(`<${tag}> is not an element here${at(where)}: the elements are ${ELEMENT_NAMES.join(', ')}, and a drawing is <svg viewBox="..."> with ${SVG_SAID} (or <Canvas> with its shapes)${instead}`);
}

/** The error for an `<svg>` that says neither how big its drawing is nor its box: its view could be anything. */
export function svgSizeError(props: Record<string, unknown>, where?: string): TypeError | undefined {
  if (props.viewBox !== undefined && props.viewBox !== null) return undefined;
  const w = length(props.width);
  const h = length(props.height);
  if (typeof w === 'number' && typeof h === 'number' && w > 0 && h > 0) return undefined;
  const why = w === '%' || h === '%' ? 'a width or height that is a share of the room says nothing of the drawing\'s coordinates' : 'without them its coordinates could be anything';
  return new TypeError(`<svg> needs a viewBox, or a width and a height in points${at(where)}: ${why}; say viewBox="0 0 200 100"`);
}

/** Paint passed down a drawing, in the wire's names, as a `<g fill="series1">` says it for what it holds. */
export interface Paint {
  fill?: string; stroke?: string; strokeWidth?: number; fillOpacity?: number; strokeOpacity?: number; fillRule?: string;
  size?: number; bold?: boolean; anchor?: string;
}

const ink = (v: unknown) => v === 'currentColor' ? 'ink' : String(v);
const num = (v: unknown) => { const n = length(v); return typeof n === 'number' ? n : undefined; };
const bold = (v: unknown) => v === 'bold' || (isNumeric(v) && Number(v) >= 600);

/** What a `<g>` or `<svg>`'s attributes add to the paint it was given. */
function svgPaint(props: Record<string, unknown>, given: Paint): Paint {
  const paint: Paint = { ...given };
  const has = (name: string) => props[name] !== undefined && props[name] !== null;
  if (has('fill')) paint.fill = ink(props.fill);
  if (has('stroke')) paint.stroke = ink(props.stroke);
  if (has('stroke-width')) paint.strokeWidth = num(props['stroke-width']);
  if (has('fill-opacity')) paint.fillOpacity = Number(props['fill-opacity']);
  if (has('stroke-opacity')) paint.strokeOpacity = Number(props['stroke-opacity']);
  if (has('fill-rule')) paint.fillRule = String(props['fill-rule']);
  if (has('font-size')) paint.size = num(props['font-size']);
  if (has('font-weight')) paint.bold = bold(props['font-weight']);
  if (has('text-anchor')) paint.anchor = String(props['text-anchor']);
  return paint;
}

/** A shape's paint, all said: its own, else what it was given, else SVG's (a fill of ink, except an open line's; no stroke). */
function shapePaint(type: string, own: Record<string, unknown>, given: Paint, closed: boolean): Record<string, unknown> {
  // A line has no inside, so a fill its group says is not its.
  const fill = own.fill ?? (type === 'Line' ? undefined : given.fill) ?? (closed ? 'ink' : 'none');
  const stroke = own.stroke ?? given.stroke ?? 'none';
  const out: Record<string, unknown> = { fill, stroke };
  if (stroke !== 'none') out.strokeWidth = own.strokeWidth ?? given.strokeWidth ?? 1;
  const opacity = own.opacity === undefined ? 1 : Number(own.opacity);
  const fillOpacity = Number(own.fillOpacity ?? given.fillOpacity ?? 1) * opacity;
  const strokeOpacity = Number(own.strokeOpacity ?? given.strokeOpacity ?? 1) * opacity;
  if (fillOpacity !== 1 && fill !== 'none') out.fillOpacity = clean(fillOpacity);
  if (strokeOpacity !== 1 && stroke !== 'none') out.strokeOpacity = clean(strokeOpacity);
  if (type === 'Path' && (own.fillRule ?? given.fillRule) !== undefined) out.fillRule = own.fillRule ?? given.fillRule;
  return out;
}

/** A transform as the wire's matrix (checked as it was set, so it parses). */
const matrix = (v: unknown): Affine | undefined => { if (v === undefined || v === null) return undefined; const m = parseTransform(v); return typeof m === 'string' ? undefined : m; };

/** Copies the entries of `from` that are said into `into`. */
const defined = (into: Record<string, unknown>, from: Record<string, unknown>) => {
  for (const [name, value] of Object.entries(from)) if (value !== undefined && value !== null) into[name] = value;
  return into;
};

/**
 * A drawing node as the app reads it: its type, its props and the paint it passes to its children. `from` is the SVG tag it was
 * written as, if it was one; `props` are as it was given them (JSON already, functions kept as handler ids). An error names `where`.
 */
export function drawWire(type: string, from: string | undefined, props: Record<string, unknown>, given: Paint, where?: string): { props: Record<string, unknown>; paint: Paint } {
  if (from) return svgWire(from, props, given, where);
  const own: Record<string, unknown> = {};
  for (const [name, value] of Object.entries(props)) own[name] = wireValue(type, name, value);
  if (type === 'Canvas') {
    if (own.view !== undefined) own.view = numbers(own.view);
    else if (typeof own.width === 'number' && typeof own.height === 'number') own.view = [0, 0, own.width, own.height];
    else throw new TypeError(`<Canvas> needs a view, or a width and a height${at(where)}: view="0 0 200 100" says its coordinates`);
    return { props: own, paint: given };
  }
  if (type === 'Group') {
    if (own.transform !== undefined) own.transform = matrix(own.transform);
    return { props: own, paint: given };
  }
  if (type === 'Text') {
    own.fill ??= given.fill === undefined || given.fill === 'none' ? 'ink' : given.fill;
    if (own.anchor === undefined && given.anchor) own.anchor = given.anchor;
    if (own.size === undefined && given.size !== undefined) own.size = given.size;
    if (own.bold === undefined && given.bold !== undefined) own.bold = given.bold;
    return { props: own, paint: given };
  }
  if (SHAPES.has(type)) {
    if (own.transform !== undefined) own.transform = matrix(own.transform);
    if (type === 'Polyline' && own.points !== undefined) own.points = numbers(own.points);
    const closed = !(type === 'Line' || (type === 'Polyline' && own.closed !== true));
    return { props: { ...own, ...shapePaint(type, own, given, closed) }, paint: given };
  }
  return { props: own, paint: given };
}

/** An SVG node (`from` its tag) as the Canvas element it is sent as. */
function svgWire(tag: string, props: Record<string, unknown>, given: Paint, where?: string): { props: Record<string, unknown>; paint: Paint } {
  const tip = typeof props.tip === 'string' ? { tip: props.tip } : {};
  const transform = matrix(props.transform);
  const n = (name: string) => num(props[name]);
  switch (tag) {
    case 'svg': {
      const error = svgSizeError(props, where);
      if (error) throw error;
      const w = length(props.width);
      const h = length(props.height);
      const view = props.viewBox !== undefined && props.viewBox !== null ? numbers(props.viewBox)! : [0, 0, w as number, h as number];
      return { props: defined({ view }, { width: typeof w === 'number' ? w : undefined, height: typeof h === 'number' ? h : undefined, ...tip }), paint: svgPaint(props, given) };
    }
    case 'g': {
      const opacity = props.opacity === undefined || props.opacity === null ? undefined : Number(props.opacity);
      return { props: defined({}, { transform, opacity, ...tip }), paint: svgPaint(props, given) };
    }
    case 'text': {
      const paint = svgPaint(props, given);
      const fill = paint.fill === undefined || paint.fill === 'none' ? 'ink' : paint.fill;
      return { props: defined({ fill }, { x: n('x') ?? 0, y: n('y') ?? 0, anchor: paint.anchor, size: paint.size, bold: paint.bold, text: props.text, ...tip }), paint: given };
    }
  }
  // A shape: its own paint in the wire's names, then filled in from what its group said.
  const own = svgPaint(props, {});
  const style: Record<string, unknown> = { ...own, opacity: props.opacity === undefined || props.opacity === null ? undefined : Number(props.opacity) };
  const geometry: Record<string, unknown> = {};
  let closed = true;
  switch (tag) {
    case 'path': geometry.d = props.d ?? ''; break;
    case 'rect': {
      Object.assign(geometry, { x: n('x') ?? 0, y: n('y') ?? 0, width: n('width') ?? 0, height: n('height') ?? 0 });
      const radius = Math.max(n('rx') ?? 0, n('ry') ?? 0);
      if (radius > 0) geometry.radius = radius;
      break;
    }
    case 'circle': Object.assign(geometry, { cx: n('cx') ?? 0, cy: n('cy') ?? 0, r: n('r') ?? 0 }); break;
    case 'ellipse': Object.assign(geometry, { cx: n('cx') ?? 0, cy: n('cy') ?? 0, rx: n('rx') ?? 0, ry: n('ry') ?? 0 }); break;
    case 'line': Object.assign(geometry, { x1: n('x1') ?? 0, y1: n('y1') ?? 0, x2: n('x2') ?? 0, y2: n('y2') ?? 0 }); closed = false; break;
    case 'polyline': case 'polygon':
      geometry.points = numbers(props.points ?? '') ?? [];
      geometry.closed = tag === 'polygon';
      closed = tag === 'polygon';
      break;
  }
  return { props: defined({ ...geometry, ...shapePaint(SVG_TAGS[tag]!, style, given, closed) }, { transform, ...tip }), paint: given };
}
