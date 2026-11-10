import { join } from 'node:path';
import type { Page } from '../cdp.ts';
import { mapPage, type MapSpec } from './map.ts';

/*
 * The live half of the `Map` element (map.ts has its rules and its page): the view of the harness browser a Map is drawn as.
 *
 * How the page reaches the view: this kernel serves it over HTTP on 127.0.0.1, a port the system picks, under a path only this
 * kernel knows (a random token, so no other program on the machine can read a cell's places), and opens a view at that URL. Why
 * a server and not `Page.setDocumentContent` on about:blank or a `Fetch.fulfillRequest` on a made-up origin: the page then has a
 * real http origin, so its tile requests carry a Referer, which OpenStreetMap's tile servers require of a browser; and it is an
 * ordinary page the pane can show and inspect. The server holds nothing but the specs of this kernel's maps and answers nothing
 * but their pages. A page needs it once, to load: it is one document with MapLibre GL inline, so a map already drawn keeps working
 * (panning, zooming, new tiles) after this kernel is gone; only reloading it then fails.
 *
 * Lifetime: the view is opened `keep`, as `browser.open`'s is, so the card stays live after the cell finishes and after this
 * kernel's connection drops. It is closed when its element leaves the cell's UI (tree.ts), and the oldest is closed when one
 * kernel holds more than MAX_MAP_VIEWS.
 */

/** The most map views one kernel keeps open: each is a page of Chromium. Opening one more closes the oldest. */
export const MAX_MAP_VIEWS = 16;
/** A map view's size in points until the app draws it (the card takes its shape from the view's). */
const VIEW_SIZE: [number, number] = [800, 480];
/** Where the vendored MapLibre GL is (vendor/maplibre-gl-4.7.1, from npm's maplibre-gl-4.7.1.tgz, sha256 7e8a778c…65f923:
 *  dist/maplibre-gl.js, dist/maplibre-gl.css, LICENSE.txt). */
const MAPLIBRE = join(import.meta.dir, '..', '..', 'vendor', 'maplibre-gl-4.7.1');

/** What a Map is drawn as now: nothing yet, the view of its page, or why there is none. */
export type MapShown = { state: 'opening' } | { state: 'open'; view: string } | { state: 'failed'; why: string };

interface MapCard {
  id: string;
  /** The spec its page should show, as JSON. */
  want: string;
  shown: MapShown;
  page?: Page;
  /** The updates sent to the page, one after another. */
  chain: Promise<void>;
  changed(): void;
  /** Its element is gone: a view that opens after this is closed at once. */
  dropped: boolean;
}

const token = crypto.randomUUID();
/** The spec of each map's page, by its id in the URL, as JSON. */
const specs = new Map<string, string>();
const cards = new WeakMap<object, MapCard>();
/** The cards whose views are open or opening, oldest first. */
const kept: MapCard[] = [];
let seq = 0;
let lib: Promise<{ js: string; css: string }> | undefined;
let server: { port: number } | undefined;

/** MapLibre GL's script and styles, read once; a missing file is an error naming it. */
function maplibre(): Promise<{ js: string; css: string }> {
  return lib ??= (async () => {
    const read = async (name: string) => {
      const file = Bun.file(join(MAPLIBRE, name));
      if (!await file.exists()) throw new Error(`the map library is missing: ${join(MAPLIBRE, name)} (vendored MapLibre GL 4.7.1, in the repository under crates/harness/mcp/vendor)`);
      return file.text();
    };
    const [js, css] = await Promise.all([read('maplibre-gl.js'), read('maplibre-gl.css')]);
    return { js, css };
  })().catch((error) => { lib = undefined; throw error; });
}

/** The URL of map `id`'s page, the server started on first use (never keeping the kernel alive on its own). */
function urlOf(id: string): string {
  if (!server) {
    const s = Bun.serve({
      hostname: '127.0.0.1',
      port: 0,
      async fetch(request) {
        const path = new URL(request.url).pathname;
        const found = /^\/([^/]+)\/([^/]+)$/.exec(path);
        const spec = request.method === 'GET' && found && found[1] === token ? specs.get(found[2]!) : undefined;
        if (spec === undefined) return new Response('no such map', { status: 404 });
        try {
          return new Response(mapPage(JSON.parse(spec) as MapSpec, await maplibre()), {
            headers: { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store', 'x-content-type-options': 'nosniff' },
          });
        } catch (error) {
          return new Response((error as Error).message, { status: 500 });
        }
      },
    });
    s.unref();
    if (typeof s.port !== 'number') throw new Error('the map server did not get a port on 127.0.0.1');
    server = { port: s.port };
  }
  return `http://127.0.0.1:${server.port}/${token}/${id}`;
}

const fail = (card: MapCard, why: string) => {
  card.shown = { state: 'failed', why };
  const at = kept.indexOf(card);
  if (at >= 0) kept.splice(at, 1);
  specs.delete(card.id);
  card.changed();
};

/** Close a card's view (its page gone from the browser's tabs) and say `why` where it was. */
function close(card: MapCard, why: string): void {
  card.dropped = true;
  void card.page?.close();
  fail(card, why);
}

/** Open the view of `card` at its page, and check the page drew a map. */
async function open(card: MapCard): Promise<void> {
  const loaded = card.want;
  // Read before the view opens, so a missing library is this error and not a page that says 500.
  await maplibre();
  const url = urlOf(card.id);
  // Loaded when a Map is first drawn: the UI's own modules stay free of the browser's (a kernel that draws no Map loads none of it here).
  const { newPage } = await import('../cdp.ts');
  const page = await newPage({ url, keep: true, size: VIEW_SIZE });
  if (card.dropped) { await page.close(); return; }
  card.page = page;
  const drew = await page.evaluate(() => typeof (window as any).harnessMap === 'object');
  if (!drew) {
    await page.close();
    throw new Error(`view ${page.view} did not draw the map page at ${url} (the harness browser could not load it from this kernel, or its script failed: open the view in the browser pane to see)`);
  }
  card.shown = { state: 'open', view: page.view! };
  if (card.want !== loaded) send(card);
  card.changed();
}

/** Have the open page draw the card's latest spec, after any update before it. */
function send(card: MapCard): void {
  const want = card.want;
  card.chain = card.chain.then(async () => {
    if (card.want !== want || card.shown.state !== 'open') return;
    try {
      await card.page!.evaluate((spec) => (window as any).harnessMap.set(spec), JSON.parse(want) as MapSpec);
    } catch (error) {
      fail(card, `the map's view ${card.page!.view} did not take its new places: ${(error as Error).message}`);
    }
  });
}

/**
 * What the Map `key` (its node in a tree) with `spec` is drawn as now. The first call opens its view; `changed` is called when what
 * it is drawn as changes (the view opened, or failed). A later call with another spec has the same page draw it.
 */
export function mapShown(key: object, spec: MapSpec, changed: () => void): MapShown {
  const want = JSON.stringify(spec);
  let card = cards.get(key);
  if (!card) {
    card = { id: `m${++seq}`, want, shown: { state: 'opening' }, chain: Promise.resolve(), changed, dropped: false };
    cards.set(key, card);
    specs.set(card.id, want);
    kept.push(card);
    while (kept.length > MAX_MAP_VIEWS) close(kept[0]!, `this map was closed: ${MAX_MAP_VIEWS} newer maps are open in this kernel (draw it again to see it)`);
    const mine = card;
    void open(mine).catch((error) => { if (!mine.dropped) fail(mine, `the map could not be opened: ${(error as Error).message}`); });
    return card.shown;
  }
  card.changed = changed;
  if (card.want !== want && card.shown.state !== 'failed') {
    card.want = want;
    specs.set(card.id, want);
    if (card.shown.state === 'open') send(card);
  }
  return card.shown;
}

/** The Map `key` left its cell's UI: its view is closed. */
export function mapGone(key: object): void {
  const card = cards.get(key);
  if (!card) return;
  cards.delete(key);
  if (card.shown.state !== 'failed') close(card, 'its element is gone');
}
