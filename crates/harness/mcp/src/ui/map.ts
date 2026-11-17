/*
 * The `Map` element's own rules. `<Map markers={places} />` is Apple's map, live in the transcript: the app covers the element's
 * card with a MapKit view (harness-native/src/window/maps.rs) the person pans and zooms, so the kernel only says what it shows.
 * This file is what the props must be (`markersOf`, `pointOf`, `mapOf`) and the element as it goes to the app (`mapWire`).
 */

/** The most places one Map shows. */
export const MAX_MARKERS = 200;
/** The most characters of a place's label, and of its detail line. */
export const MAX_MAP_LABEL = 120;
export const MAX_MAP_DETAIL = 400;
/**
 * `zoom` is a web map's (256-pixel tiles, as OpenStreetMap's and Google's count it): 0 the world, 19 a building. The app turns it
 * into the span MapKit takes: a view `w` points wide shows `360 * w / (256 * 2 ** zoom)` degrees of longitude (maps.rs `span_of`).
 */
export const MAX_MAP_ZOOM = 19;

/** One place: where it is, the words beside its pin, a line more and a link shown when it is pressed. */
export interface MapMarker { lat: number; lon: number; label?: string; detail?: string; href?: string; place?: string }
/** A Map as the app reads it: every place checked, points as [lat, lon]. */
export interface MapSpec { markers: MapMarker[]; center?: [number, number]; zoom?: number; here?: [number, number] }

const said = (v: unknown): string => {
  const text = typeof v === 'string' || (typeof v === 'object' && v !== null) ? JSON.stringify(v) : String(v);
  return text.length > 60 ? `${text.slice(0, 59)}…` : text;
};
const finite = (v: unknown): v is number => typeof v === 'number' && Number.isFinite(v);
const MARKER_SAID = '{lat, lon, label?, detail?, href?, place?}';
const MARKER_KEYS = ['lat', 'lon', 'label', 'detail', 'href', 'place'];
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

/** The problem with `value` as a Map's `markers`, or the places as the app reads them: at most MAX_MARKERS, each MARKER_SAID. */
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
    // The place as Apple Maps knows it (places()'s `id`): its pin shows Apple's own card of it, with its pictures and hours.
    if (o.place !== undefined && o.place !== null && o.place !== '') {
      if (typeof o.place !== 'string' || !/^[A-Za-z0-9]{1,64}$/.test(o.place)) return `${what} has place ${said(o.place)}: a place's id as places() gives it, place: p.id`;
      marker.place = o.place;
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
 * A Map's props as the app reads them, or the problem (said after `<Map> `): the places, and where it looks. With no `center` it is
 * fitted to the places (to `here` when there are none); one with nothing to look at is the problem.
 */
export function mapOf(props: Record<string, unknown>): MapSpec | string {
  const given = (name: string) => props[name] !== undefined && props[name] !== null && props[name] !== '';
  const markers = given('markers') ? markersOf(props.markers) : [];
  if (typeof markers === 'string') return `has ${markers}`;
  const spec: MapSpec = { markers };
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
  return spec;
}

/**
 * The Map's own props on the wire, `{type: 'Map', props}` (the app's genui/map.rs checks and reads exactly these): `markers` always,
 * a list of `{lat, lon, label?, detail?, href?, place?}`; `here` and `center` as `[lat, lon]` and `zoom` as a number, each only when said.
 */
export function mapWire(spec: MapSpec): Record<string, unknown> {
  const out: Record<string, unknown> = { markers: spec.markers };
  if (spec.here) out.here = spec.here;
  if (spec.center) out.center = spec.center;
  if (spec.zoom !== undefined) out.zoom = spec.zoom;
  return out;
}
