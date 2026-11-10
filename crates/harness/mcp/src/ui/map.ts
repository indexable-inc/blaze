/*
 * The `Map` element's own rules and its page. `<Map markers={places} />` is no drawing of the app's: the kernel serves one
 * self-contained page (MapLibre GL, vendored in vendor/maplibre-gl-4.7.1, over OpenFreeMap's vector tiles of OpenStreetMap; no key) and shows it in a view
 * of the harness browser, so the element goes to the app as the `Browser` element of that view (map-view.ts opens it, tree.ts
 * lowers it). This file is the pure half: what the props must be (`markersOf`, `pointOf`, `mapOf`) and the page's HTML (`mapPage`).
 */

/** The most places one Map shows. */
export const MAX_MARKERS = 200;
/** The most characters of a place's label, and of its detail line. */
export const MAX_MAP_LABEL = 120;
export const MAX_MAP_DETAIL = 400;
/** OpenStreetMap's tiles go from zoom 0 (the world) to 19 (a building). */
export const MAX_MAP_ZOOM = 19;
/** Past this many places their labels show under the pointer only, so they do not cover the map. */
export const DENSE_MARKERS = 30;

/** One place: where it is, the words beside its pin, a line more and a link shown when it is pressed. */
export interface MapMarker { lat: number; lon: number; label?: string; detail?: string; href?: string }
/** A Map as its page reads it: every place checked, points as [lat, lon]. */
export interface MapSpec { markers: MapMarker[]; center?: [number, number]; zoom?: number; here?: [number, number]; title: string }

const said = (v: unknown): string => {
  const text = typeof v === 'string' || (typeof v === 'object' && v !== null) ? JSON.stringify(v) : String(v);
  return text.length > 60 ? `${text.slice(0, 59)}…` : text;
};
const finite = (v: unknown): v is number => typeof v === 'number' && Number.isFinite(v);
const MARKER_SAID = '{lat, lon, label?, detail?, href?}';
const MARKER_KEYS = ['lat', 'lon', 'label', 'detail', 'href'];
/** What a place's key was most likely meant to be. */
const MARKER_MEANT: Readonly<Record<string, string>> = {
  lng: 'lon', long: 'lon', longitude: 'lon', latitude: 'lat', name: 'label', title: 'label', text: 'label', description: 'detail', note: 'detail',
  url: 'href', link: 'href',
};

/** Why `lat` and `lon` are no point of the earth, `what` naming whose they are; undefined when they are one. */
function coordsProblem(lat: unknown, lon: unknown, what: string): string | undefined {
  if (!finite(lat) || lat < -90 || lat > 90) return `${what} has lat ${said(lat)}: a latitude is a number from -90 to 90`;
  if (!finite(lon) || lon < -180 || lon > 180) return `${what} has lon ${said(lon)}: a longitude is a number from -180 to 180`;
  return undefined;
}

/**
 * A point as [lat, lon], or the problem with `value` as one (`what` names the prop): it is `[lat, lon]`, `{lat, lon}`, or what
 * `location()` returns, `{latitude, longitude, ...}`.
 */
export function pointOf(value: unknown, what: string): [number, number] | string {
  const wants = `${what}={[lat, lon]} (or {lat, lon}, or what location() returns)`;
  let lat: unknown;
  let lon: unknown;
  if (Array.isArray(value)) {
    if (value.length !== 2) return `${what} has ${value.length} numbers: a point is two, ${wants}`;
    [lat, lon] = value;
  } else if (typeof value === 'object' && value !== null) {
    const o = value as Record<string, unknown>;
    lat = o.lat ?? o.latitude;
    lon = o.lon ?? o.longitude;
    if (lat === undefined || lon === undefined) return `${what} is ${said(value)}, which names no ${lat === undefined ? 'lat' : 'lon'}: ${wants}`;
  } else {
    return `${what} is ${said(value)}: a point is ${wants}`;
  }
  return coordsProblem(lat, lon, what) ?? [lat as number, lon as number];
}

/** The problem with `value` as a Map's `markers`, or the places as its page reads them: at most MAX_MARKERS, each MARKER_SAID. */
export function markersOf(value: unknown): MapMarker[] | string {
  const example = `markers={[{lat: 34.1, lon: -118.1, label: 'Cha Redefine'}]}`;
  if (!Array.isArray(value)) return `markers is ${said(value)}, not a list: ${example}`;
  if (value.length > MAX_MARKERS) return `markers has ${value.length} places; a Map shows at most ${MAX_MARKERS} (pick the nearest, or show the rest as a Table)`;
  const out: MapMarker[] = [];
  for (const [i, item] of value.entries()) {
    const what = `markers[${i}]`;
    if (typeof item !== 'object' || item === null || Array.isArray(item)) return `${what} is ${said(item)}: a place is ${MARKER_SAID}`;
    const o = item as Record<string, unknown>;
    const stray = Object.keys(o).find(key => !MARKER_KEYS.includes(key));
    if (stray) return `${what} has ${JSON.stringify(stray)}${MARKER_MEANT[stray] ? ` (say ${MARKER_MEANT[stray]})` : ''}: a place is ${MARKER_SAID}`;
    const bad = coordsProblem(o.lat, o.lon, what);
    if (bad) return bad;
    const marker: MapMarker = { lat: o.lat as number, lon: o.lon as number };
    for (const [name, max] of [['label', MAX_MAP_LABEL], ['detail', MAX_MAP_DETAIL]] as const) {
      const words = o[name];
      if (words === undefined || words === null || words === '') continue;
      if (typeof words !== 'string' && !finite(words)) return `${what} has ${name} ${said(words)}: words (a string)`;
      if (String(words).length > max) return `${what} has a ${name} of ${String(words).length} characters: at most ${max}`;
      marker[name] = String(words);
    }
    if (o.href !== undefined && o.href !== null && o.href !== '') {
      if (typeof o.href !== 'string' || !/^https?:\/\/\S+$/i.test(o.href.trim())) return `${what} has href ${said(o.href)}: an http(s) URL, href: 'https://example.com'`;
      marker.href = o.href.trim();
    }
    out.push(marker);
  }
  return out;
}

/** The problem with `value` as a Map's `zoom`, or the zoom. */
function zoomOf(value: unknown): number | string {
  const n = typeof value === 'string' && /^\s*\d+(\.\d+)?\s*$/.test(value) ? Number(value) : value;
  return finite(n) && n >= 0 && n <= MAX_MAP_ZOOM ? n : `zoom is ${said(value)}: a number from 0 (the world) to ${MAX_MAP_ZOOM} (a building)`;
}

/** What each of a Map's props must be, as element.ts's rules say it (`... is not <this>`); undefined when it is fine. */
export const MAP_RULES = {
  markers: (v: unknown): string | undefined => { const m = markersOf(v); return typeof m === 'string' ? `a list of at most ${MAX_MARKERS} places, each ${MARKER_SAID}: ${m}` : undefined; },
  center: (v: unknown): string | undefined => { const p = pointOf(v, 'center'); return typeof p === 'string' ? `a point of the earth: ${p}` : undefined; },
  here: (v: unknown): string | undefined => { const p = pointOf(v, 'here'); return typeof p === 'string' ? `a point of the earth: ${p}` : undefined; },
  zoom: (v: unknown): string | undefined => { const z = zoomOf(v); return typeof z === 'string' ? `a zoom level: ${z}` : undefined; },
};

/**
 * A Map's props as its page reads them, or the problem (said after `<Map> `): the places, and where it looks. With no `center` it is
 * fitted to the places (to `here` when there are none); one with nothing to look at is the problem.
 */
export function mapOf(props: Record<string, unknown>): MapSpec | string {
  const given = (name: string) => props[name] !== undefined && props[name] !== null && props[name] !== '';
  const markers = given('markers') ? markersOf(props.markers) : [];
  if (typeof markers === 'string') return `has ${markers}`;
  const spec: MapSpec = { markers, title: '' };
  for (const name of ['center', 'here'] as const) {
    if (!given(name)) continue;
    const point = pointOf(props[name], name);
    if (typeof point === 'string') return `has ${point}`;
    spec[name] = point;
  }
  if (given('zoom')) {
    const zoom = zoomOf(props.zoom);
    if (typeof zoom === 'string') return `has ${zoom}`;
    spec.zoom = zoom;
  }
  if (markers.length === 0 && !spec.center && !spec.here) {
    return `has nothing to show: say markers={[{lat, lon, label}]} (it is fitted to them), center={[lat, lon]} with a zoom, or here={[lat, lon]}`;
  }
  const named = markers.filter(m => m.label);
  spec.title = markers.length === 1 && named.length === 1 ? named[0]!.label!
    : markers.length > 0 ? `Map of ${markers.length} places${named.length ? `: ${named.slice(0, 3).map(m => m.label).join(', ')}${named.length > 3 ? ', …' : ''}` : ''}`
    : 'Map';
  return spec;
}

/** Text as HTML text. */
const html = (text: string): string => text.replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]!);
/** JSON that can sit inside a <script> element: no `<` to end it early, no line separators JavaScript once refused. */
const inScript = (value: unknown): string => JSON.stringify(value).replace(/[<\u2028\u2029]/g, c => `\\u${c.charCodeAt(0).toString(16).padStart(4, '0')}`);

/** The map's style: OpenFreeMap's Liberty (OpenMapTiles data from OpenStreetMap; no key, no account), a map in colour as
 *  Apple's Maps reads: blue water, green parks, warm main roads, places with their icons. Drawn by MapLibre GL as vectors at
 *  the screen's own resolution. On the light scheme it is shown as it is; on the dark one the page turns every colour of it to
 *  its dark counterpart (pageMain's `night`): the hue kept, the lightness turned over, so water stays blue and parks green. */
export const MAP_STYLE = 'https://tiles.openfreemap.org/styles/liberty';

/** The page around the map, in the scheme's colours (`body.light` on the light one): red pins with their words, the person's
 *  place as a blue dot ringed in white. */
const PAGE_CSS = `
html, body, #map { height: 100%; margin: 0; }
body { --ground: #26282C; --ink: #E6E8EB; --muted: #9AA0A8; --plate: #2B2F36; --line: #3B414B; background: var(--ground); color: var(--ink); font: 12px/16px -apple-system, "SF Pro Text", system-ui, sans-serif; -webkit-font-smoothing: antialiased; }
body.light { --ground: #F4F2EC; --ink: #1D1F23; --muted: #6B6F76; --plate: #FFFFFF; --line: #DAD5CA; }
.maplibregl-map { background: var(--ground); font: inherit; color: inherit; }
.maplibregl-canvas { outline: none; }
.maplibregl-ctrl-attrib { background: color-mix(in srgb, var(--ground) 78%, transparent) !important; color: var(--muted); font-size: 10px; line-height: 14px; }
.maplibregl-ctrl-attrib a { color: var(--muted); text-decoration: none; }
body:not(.light) .maplibregl-ctrl-attrib-button { filter: invert(1); opacity: 0.6; }
.pin { width: 14px; height: 14px; box-sizing: border-box; border-radius: 50%; background: #FF4F45; border: 2px solid #FFFFFF; box-shadow: 0 1px 3px rgba(0, 0, 0, 0.35); cursor: default; }
.pin.pressable { cursor: pointer; }
.pin b { position: absolute; left: 16px; top: -4px; white-space: nowrap; font-weight: 600; color: var(--ink);
  text-shadow: 0 0 2px var(--ground), 0 0 2px var(--ground), 0 0 4px var(--ground), 0 0 4px var(--ground); }
.dense .pin b { display: none; }
.dense .pin:hover b { display: block; }
.maplibregl-marker:hover { z-index: 3; }
.here { width: 16px; height: 16px; box-sizing: border-box; border-radius: 50%; background: #2F7BF6; border: 3px solid #FFFFFF; box-shadow: 0 0 0 5px rgba(47, 123, 246, 0.22), 0 1px 3px rgba(0, 0, 0, 0.35); }
.maplibregl-popup-content { background: var(--plate); color: var(--ink); border: 1px solid var(--line); border-radius: 8px; box-shadow: none; padding: 8px 10px; font: inherit; line-height: 16px; max-width: 260px; }
.maplibregl-popup-content strong { display: block; font-weight: 600; }
.maplibregl-popup-content span { display: block; color: var(--muted); }
.maplibregl-popup-content a { display: block; margin-top: 4px; color: #2F7BF6; text-decoration: none; }
.maplibregl-popup-tip { display: none; }
`;

/**
 * What the page runs once MapLibre is loaded: the map (a fit or a zoom is set, never flown to; nothing fades in), the places as
 * pins with their words, `here` as a ringed dot. `harnessMap.set(spec)` draws another spec in place (the kernel calls it when the
 * element's props change). Until the person moves the map, it is fitted again whenever its view changes size. `zoom` is a web
 * map's (256-pixel tiles, as OpenStreetMap's and Google's count it): MapLibre's own is one less.
 */
async function pageMain(DENSE: number, STYLE: string): Promise<void> {
  type Spec = { markers: Array<{ lat: number; lon: number; label?: string; detail?: string; href?: string }>; center?: [number, number]; zoom?: number; here?: [number, number]; title: string };
  const M = (window as any).maplibregl;
  const fresh = JSON.parse(document.getElementById('spec')!.textContent!) as Spec;
  // The style is fetched here, retinted, and handed over whole, so the map never shows in the style's own colours first.
  const got = await fetch(STYLE);
  if (!got.ok) { document.body.textContent = `The map's style could not be loaded (${STYLE}: ${got.status}).`; return; }
  const style = await got.json();
  const light = !matchMedia('(prefers-color-scheme: dark)').matches;
  document.body.classList.toggle('light', light);
  if (!light) {
    // A CSS colour as hue (degrees), saturation, lightness and alpha; undefined for what is no colour (an expression's
    // operator, a property name).
    const hsla = (c: string): [number, number, number, number] | undefined => {
      const t = c.trim().toLowerCase();
      let m = /^#([0-9a-f]{3,8})$/.exec(t);
      let rgb: number[] | undefined;
      let alpha = 1;
      if (m && [3, 4, 6, 8].includes(m[1]!.length)) {
        const h = m[1]!.length <= 4 ? [...m[1]!].map(x => x + x).join('') : m[1]!;
        rgb = [0, 2, 4].map(i => parseInt(h.slice(i, i + 2), 16) / 255);
        if (h.length === 8) alpha = parseInt(h.slice(6, 8), 16) / 255;
      } else if ((m = /^rgba?\(([^)]+)\)$/.exec(t))) {
        const p = m[1]!.split(/[\s,/]+/).filter(Boolean).map(Number);
        if (p.length < 3 || p.some(Number.isNaN)) return undefined;
        rgb = p.slice(0, 3).map(x => x / 255);
        alpha = p[3] ?? 1;
      } else if ((m = /^hsla?\(([^)]+)\)$/.exec(t))) {
        const p = m[1]!.split(/[\s,/]+/).filter(Boolean).map(x => parseFloat(x));
        if (p.length < 3 || p.some(Number.isNaN)) return undefined;
        return [p[0]!, p[1]! / 100, p[2]! / 100, p[3] ?? 1];
      } else if (t === 'white' || t === 'black') {
        rgb = t === 'white' ? [1, 1, 1] : [0, 0, 0];
      }
      if (!rgb) return undefined;
      const [r, g, b] = rgb as [number, number, number];
      const max = Math.max(r, g, b), min = Math.min(r, g, b), l = (max + min) / 2, d = max - min;
      if (d === 0) return [0, 0, l, alpha];
      const sat = d / (1 - Math.abs(2 * l - 1));
      const hue = max === r ? ((g - b) / d + 6) % 6 : max === g ? (b - r) / d + 2 : (r - g) / d + 4;
      return [hue * 60, sat, l, alpha];
    };
    // A colour's dark counterpart: the lightness turned over into the dark scheme's range (paper becomes a slate ground,
    // ink becomes light words) and the hue kept where it says something: water stays blue, a park green (Liberty's
    // yellow-greens pulled to one green), a motorway amber. Liberty's warm paper, buildings and yellow main roads would
    // turn to browns, so warm colours lose theirs. A street is set by its kind, not turned: white on paper would sink
    // under the ground, so it is a grey a step over the ground, its casing under.
    type Road = 'street' | 'casing' | 'motorway' | 'motorway-casing' | undefined;
    const night = (c: string, road: Road): string | undefined => {
      const v = hsla(c);
      if (!v) return undefined;
      let [h, sat, l, alpha] = v;
      let to = 0.15 + (1 - l) * 0.75;
      if (road === 'street') [sat, to] = [0.06, 0.38];
      else if (road === 'casing') [sat, to] = [0.06, 0.12];
      else if (road === 'motorway') [h, sat, to] = [36, 0.52, 0.47];
      else if (road === 'motorway-casing') [h, sat, to] = [36, 0.4, 0.2];
      else if (h >= 10 && h < 65) [h, sat] = [220, Math.min(sat, 0.5) * 0.18];
      else if (h >= 65 && h < 170) [h, sat, to] = [140, Math.min(sat, 0.6) * 0.78, Math.min(to, 0.3)];
      else sat = Math.min(1, sat * 0.85);
      return `hsla(${h.toFixed(0)}, ${(sat * 100).toFixed(0)}%, ${(to * 100).toFixed(0)}%, ${alpha})`;
    };
    const turn = (v: unknown, road: Road): unknown =>
      typeof v === 'string' ? night(v, road) ?? v
        : Array.isArray(v) ? v.map(x => turn(x, road))
        : v && typeof v === 'object' ? Object.fromEntries(Object.entries(v).map(([k, x]) => [k, turn(x, road)]))
        : v;
    for (const layer of style.layers ?? []) {
      const id = String(layer.id);
      const rail = /rail|transit|ferry|pier|path|aeroway/.test(id);
      const road: Road = layer.type !== 'line' || layer['source-layer'] !== 'transportation' || rail ? undefined
        : id.includes('motorway') ? (id.includes('casing') ? 'motorway-casing' : 'motorway')
        : id.includes('casing') ? 'casing' : 'street';
      for (const [k, v] of Object.entries(layer.paint ?? {})) if (k.endsWith('-color')) layer.paint[k] = turn(v, road);
    }
  }
  const map = new M.Map({ container: 'map', style, center: [0, 0], zoom: 1, attributionControl: false, fadeDuration: 0, dragRotate: false, pitchWithRotate: false, touchPitch: false });
  map.touchZoomRotate.disableRotation();
  map.addControl(new M.AttributionControl({ compact: true }), 'bottom-right');
  let spec = fresh;
  let moved = false;
  let drawn: any[] = [];
  const el = (tag: string, text?: string) => { const e = document.createElement(tag); if (text !== undefined) e.textContent = text; return e; };
  const at = (p: [number, number] | { lat: number; lon: number }): [number, number] => (Array.isArray(p) ? [p[1], p[0]] : [p.lon, p.lat]);
  const zoom = (z: number) => Math.max(0, z - 1);
  const fit = () => {
    if (spec.center) return map.jumpTo({ center: at(spec.center), zoom: zoom(spec.zoom ?? 13) });
    if (spec.markers.length === 0) return map.jumpTo({ center: at(spec.here!), zoom: zoom(spec.zoom ?? 14) });
    const bounds = new M.LngLatBounds();
    for (const m of spec.markers) bounds.extend(at(m));
    if (spec.zoom !== undefined) return map.jumpTo({ center: bounds.getCenter(), zoom: zoom(spec.zoom) });
    // Room for a pin's words to its right; one place alone is shown at street level, not at the deepest zoom.
    map.fitBounds(bounds, { animate: false, maxZoom: 14, padding: { top: 40, left: 32, bottom: 48, right: 96 } });
  };
  const draw = () => {
    for (const d of drawn) d.remove();
    drawn = [];
    document.title = spec.title;
    document.body.classList.toggle('dense', spec.markers.length > DENSE);
    if (spec.here) {
      const dot = el('div');
      dot.className = 'here';
      drawn.push(new M.Marker({ element: dot }).setLngLat(at(spec.here)).addTo(map));
    }
    for (const m of spec.markers) {
      const pin = el('div');
      const pressable = !!(m.detail || m.href);
      pin.className = pressable ? 'pin pressable' : 'pin';
      if (m.label) pin.append(el('b', m.label));
      const marker = new M.Marker({ element: pin }).setLngLat(at(m)).addTo(map);
      drawn.push(marker);
      if (!pressable) continue;
      const card = el('div');
      if (m.label) card.append(el('strong', m.label));
      if (m.detail) card.append(el('span', m.detail));
      if (m.href) {
        const a = el('a', 'Open') as HTMLAnchorElement;
        a.href = m.href; a.target = '_blank'; a.rel = 'noopener';
        card.append(a);
      }
      marker.setPopup(new M.Popup({ closeButton: false, offset: 10, maxWidth: '280px' }).setDOMContent(card));
    }
  };
  // The person's own drag or scroll, never a fit of ours.
  for (const type of ['pointerdown', 'wheel', 'keydown']) map.getContainer().addEventListener(type, () => { moved = true; }, { passive: true });
  map.on('resize', () => { if (!moved) fit(); });
  draw();
  fit();
  (window as any).harnessMap = { set(next: Spec) { spec = next; moved = false; draw(); fit(); return true; } };
}

/**
 * The whole page of one Map: MapLibre GL's script and styles (`lib`, the vendored files' text) inline, the spec as JSON, and the script
 * above. One document, one request; the only things it loads after are the style and its tiles, fonts and icons (tiles.openfreemap.org).
 */
export function mapPage(spec: MapSpec, lib: { js: string; css: string }): string {
  const main = `(${pageMain.toString()})(${DENSE_MARKERS}, ${JSON.stringify(MAP_STYLE)});`;
  const safe = (code: string) => code.replace(/<\/(script)/gi, '<\\/$1');
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="dark light">
<title>${html(spec.title)}</title>
<style>${lib.css.replace(/<\/(style)/gi, '<\\/$1')}</style>
<style>${PAGE_CSS}</style>
</head>
<body>
<div id="map"></div>
<script type="application/json" id="spec">${inScript(spec)}</script>
<script>${safe(lib.js.replace(/\/\/# sourceMappingURL=\S+\s*$/, ''))}</script>
<script>${safe(main)}</script>
</body>
</html>
`;
}
