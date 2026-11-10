/*
 * The `Map` element's own rules and its page. `<Map markers={places} />` is no drawing of the app's: the kernel serves one
 * self-contained page (Leaflet, vendored in vendor/leaflet-1.9.4, over OpenStreetMap's raster tiles; no key) and shows it in a view
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

/** The app's look, as the page's: near-black ground, ink only, one blue for the pins. Nothing eases. */
const PAGE_CSS = `
html, body, #map { height: 100%; margin: 0; }
body { background: #0A0B0D; color: #E6E8EB; font: 12px/16px -apple-system, "SF Pro Text", system-ui, sans-serif; -webkit-font-smoothing: antialiased; }
.leaflet-container { background: #0A0B0D; font: inherit; color: inherit; outline: none; }
.leaflet-container a { color: #4D8EFF; }
/* OpenStreetMap's light tiles as quiet dark ink: grey, inverted, pulled down so the pins and their words stand over them. */
.leaflet-tile-pane { filter: grayscale(1) invert(1) brightness(0.82) contrast(0.92); }
.leaflet-control-attribution { background: rgba(10, 11, 13, 0.78) !important; color: #9AA0A8; font-size: 10px; line-height: 14px; padding: 1px 6px; }
.leaflet-control-attribution a { color: #C4C8CE; text-decoration: none; }
.leaflet-bar { border: 1px solid #2A2D33 !important; border-radius: 6px; box-shadow: none; overflow: hidden; }
.leaflet-bar a, .leaflet-bar a:hover { background: #14161A; color: #E6E8EB; border-bottom: 1px solid #2A2D33; width: 26px; height: 26px; line-height: 26px; font-size: 16px; }
.leaflet-bar a:last-child { border-bottom: none; }
.leaflet-bar a:hover { background: #1C1F24; }
.leaflet-bar a.leaflet-disabled { background: #14161A; color: #5A6068; }
.pin { width: 0 !important; height: 0 !important; margin: 0 !important; }
.pin i { position: absolute; left: -6px; top: -6px; width: 12px; height: 12px; box-sizing: border-box; border-radius: 50%; background: #4D8EFF; border: 2px solid #0A0B0D; }
.pin b { position: absolute; left: 10px; top: -8px; white-space: nowrap; font-weight: 500; color: #E6E8EB;
  text-shadow: 0 0 2px #0A0B0D, 0 0 2px #0A0B0D, 0 0 4px #0A0B0D, 0 0 4px #0A0B0D; }
.pin { cursor: default; }
.pin.pressable { cursor: pointer; }
.dense .pin b { display: none; }
.dense .pin:hover b { display: block; }
.pin:hover { z-index: 100000 !important; }
.here i { position: absolute; left: -7px; top: -7px; width: 14px; height: 14px; box-sizing: border-box; border-radius: 50%; background: #E6E8EB; border: 3px solid #0A0B0D; box-shadow: 0 0 0 1px #E6E8EB; }
.leaflet-popup-content-wrapper { background: #14161A; color: #E6E8EB; border: 1px solid #2A2D33; border-radius: 8px; box-shadow: none; }
.leaflet-popup-content { margin: 8px 10px; font: inherit; line-height: 16px; max-width: 260px; }
.leaflet-popup-content strong { display: block; font-weight: 600; }
.leaflet-popup-content span { display: block; color: #9AA0A8; }
.leaflet-popup-content a { display: block; margin-top: 4px; text-decoration: none; }
.leaflet-popup-tip { background: #14161A; border: 1px solid #2A2D33; box-shadow: none; }
`;

/**
 * What the page runs once Leaflet is loaded: the map with every animation off (a fit or a zoom is set, never flown to), the places
 * as pins with their words, `here` as a ring. `harnessMap.set(spec)` draws another spec in place (the kernel calls it when the
 * element's props change). Until the person moves the map, it is fitted again whenever its view changes size.
 */
function pageMain(DENSE: number): void {
  type Spec = { markers: Array<{ lat: number; lon: number; label?: string; detail?: string; href?: string }>; center?: [number, number]; zoom?: number; here?: [number, number]; title: string };
  const L = (window as any).L;
  const fresh = JSON.parse(document.getElementById('spec')!.textContent!) as Spec;
  const map = L.map('map', { zoomAnimation: false, fadeAnimation: false, markerZoomAnimation: false, inertia: false, worldCopyJump: true });
  map.attributionControl.setPrefix(false);
  L.tileLayer('https://tile.openstreetmap.org/{z}/{x}/{y}.png', {
    maxZoom: 19,
    attribution: '&copy; <a href="https://www.openstreetmap.org/copyright" target="_blank" rel="noopener">OpenStreetMap</a> contributors',
  }).addTo(map);
  const layer = L.layerGroup().addTo(map);
  let spec = fresh;
  let moved = false;
  const el = (tag: string, text?: string) => { const e = document.createElement(tag); if (text !== undefined) e.textContent = text; return e; };
  const fit = () => {
    const still = { animate: false };
    if (spec.center) return map.setView(spec.center, spec.zoom ?? 13, still);
    if (spec.markers.length === 0) return map.setView(spec.here, spec.zoom ?? 14, still);
    const bounds = L.latLngBounds(spec.markers.map(m => [m.lat, m.lon]));
    if (spec.zoom !== undefined) return map.setView(bounds.getCenter(), spec.zoom, still);
    // Room for a pin's words to its right; one place alone is shown at street level, not at the deepest zoom.
    map.fitBounds(bounds, { animate: false, maxZoom: 15, paddingTopLeft: [32, 32], paddingBottomRight: [96, 32] });
  };
  const draw = () => {
    layer.clearLayers();
    document.title = spec.title;
    document.body.classList.toggle('dense', spec.markers.length > DENSE);
    if (spec.here) {
      L.marker(spec.here, { icon: L.divIcon({ className: 'pin here', html: el('i'), iconSize: null }), interactive: false, keyboard: false, zIndexOffset: -1000 }).addTo(layer);
    }
    for (const m of spec.markers) {
      const body = document.createDocumentFragment();
      body.append(el('i'));
      if (m.label) body.append(el('b', m.label));
      const holder = el('div');
      holder.append(body);
      const pressable = !!(m.detail || m.href);
      const marker = L.marker([m.lat, m.lon], {
        icon: L.divIcon({ className: pressable ? 'pin pressable' : 'pin', html: holder, iconSize: null }), keyboard: false,
      }).addTo(layer);
      if (!pressable) continue;
      const card = el('div');
      if (m.label) card.append(el('strong', m.label));
      if (m.detail) card.append(el('span', m.detail));
      if (m.href) {
        const a = el('a', 'Open') as HTMLAnchorElement;
        a.href = m.href; a.target = '_blank'; a.rel = 'noopener';
        card.append(a);
      }
      marker.bindPopup(card, { closeButton: false, offset: [0, -4], autoPan: false });
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
 * The whole page of one Map: Leaflet's script and styles (`lib`, the vendored files' text) inline, the spec as JSON, and the script
 * above. One document, one request; the only thing it loads after is OpenStreetMap's tiles.
 */
export function mapPage(spec: MapSpec, lib: { js: string; css: string }): string {
  const main = `(${pageMain.toString()})(${DENSE_MARKERS});`;
  const safe = (code: string) => code.replace(/<\/(script)/gi, '<\\/$1');
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="dark">
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
