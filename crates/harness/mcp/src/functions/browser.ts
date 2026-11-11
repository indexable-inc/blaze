// `browser.*`: the harness browser (embedded Chromium the daemon runs, crates/harness/harness-browser) as a cell uses it
// directly. googleSearch, gmail and withHiddenPage drive it too; these are for a page the person should see, and for
// what the browser holds.
import { host, newPage, withHiddenPage, type Extension, type Fix, type Page, type Place, type ViewInfo } from "../cdp.ts";
import { pmap } from "../pmap.ts";
import { currentTask, type Task } from "../tasks.ts";
import { show } from "../ui/index.ts";
import { hostComponent } from "../ui/svelte.ts";
import { MAX_MARKERS } from "../ui/map.ts";
import { table, type Table } from "../table.ts";
import { effect } from "../effects.ts";

const hostOf = (url: string) => { try { return new URL(url).host || url; } catch { return url; } };

/** Open `url` in a view of its own that stays open (in the browser pane's tabs, after this kernel too), loaded or after
 *  20 s still loading; the chat shows a line for it that opens it in the browser pane beside the chat when pressed. The Page drives it (goto, evaluate, send...). */
function open(url: string): Promise<Page> {
  const h = hostOf(url);
  return effect("net", `Opening ${h}`, `Opened ${h}`, url, () => newPage({ url, keep: true }), (p) => ({ show: p }));
}

/** Every view the harness browser has: the person's tabs, pages agents opened, popups. */
function views(): Promise<Table<ViewInfo>> {
  return effect("list", "Listing browser views", "Listed browser views", "harness browser", async () => table(await (await host()).views()), (t) => ({
    detail: `${t.length} view${t.length === 1 ? "" : "s"}`,
  }));
}

/** The extensions installed in the harness browser's profile. */
function extensions(): Promise<Table<Extension>> {
  return effect("list", "Listing browser extensions", "Listed browser extensions", "harness browser", async () => table(await (await host()).extensions()), (t) => ({
    detail: `${t.length} extension${t.length === 1 ? "" : "s"}`,
  }));
}

/** Open the manage window at `url` (chrome://extensions when none; the Chrome Web Store installs there). */
function manage(url?: string): Promise<void> {
  const at = url ?? "chrome://extensions";
  return effect("exec", `Opening ${at}`, `Opened ${at}`, at, async () => { await (await host()).manage(url); });
}

/** One page read by [`read`]: its words, or why it could not be read. */
export type Read = { url: string; title: string; text: string; error: string };

/** How many pages `read` has open at once. */
const READ_AT_ONCE = 6;

/** The words of each of `urls` (a page's innerText once it has settled, Page.text), every page in a hidden view of its
 *  own, `READ_AT_ONCE` at a time, in the order given. Reading many pages is one call that takes as long as the slowest
 *  few, not a loop over one view. A page that fails has its `error` and no text; the others are still read. */
function read(urls: string | string[], opts: { maxChars?: number; timeoutMs?: number } = {}): Promise<Table<Read>> {
  const list = typeof urls === "string" ? [urls] : urls;
  const what = list.length === 1 ? hostOf(list[0]!) : `${list.length} pages`;
  return effect("net", `Reading ${what}`, `Read ${what}`, list.join(" "), async () => table(await pmap(list, (url) =>
    withHiddenPage(async (page): Promise<Read> => {
      try {
        await page.goto(url, opts.timeoutMs ?? 20_000);
        const text = await page.text();
        return { url: await page.url(), title: await page.title(), text: opts.maxChars ? text.slice(0, opts.maxChars) : text, error: "" };
      } catch (e) {
        return { url, title: "", text: "", error: (e as Error).message };
      }
    }), { concurrency: READ_AT_ONCE })), (t) => {
    const failed = t.filter((r) => r.error).length;
    return { ok: failed < t.length, detail: failed ? `${failed} of ${t.length} failed` : "" };
  });
}

export const browser = { open, read, views, extensions, manage };

/** A place Apple Maps found, and how far it is from where the search was centred. */
export type Near = Place & { distance_m: number; maps: string };

/** Metres between two points on the Earth (haversine). */
function metres(a: { latitude: number; longitude: number }, b: { latitude: number; longitude: number }): number {
  const rad = (d: number) => (d * Math.PI) / 180;
  const h = Math.sin(rad(b.latitude - a.latitude) / 2) ** 2 + Math.cos(rad(a.latitude)) * Math.cos(rad(b.latitude)) * Math.sin(rad(b.longitude - a.longitude) / 2) ** 2;
  return 2 * 6_371_000 * Math.asin(Math.sqrt(h));
}

/** What each cell's place searches found so far, and where the person is when a search was centred on them: the one
 *  map the cell draws shows all of it (two searches in one cell are one map, not the second replacing the first). */
const drawn = new WeakMap<Task, { here?: [number, number]; found: Near[] }>();

/** Draw (or redraw) the calling cell's map of the places its searches found: a pin each, nearest first, each naming
 *  its place so its pin shows Apple's own card of it; the person's place as the blue dot. The cell's own render(), or
 *  markup, replaces it. */
function mapOf(found: Near[], here: [number, number] | undefined) {
  const t = currentTask();
  if (!t || found.length === 0) return;
  const all = drawn.get(t) ?? { found: [] };
  drawn.set(t, all);
  all.here ??= here;
  for (const p of found) if (!all.found.some((q) => (p.id ? q.id === p.id : q.name === p.name && q.distance_m === p.distance_m))) all.found.push(p);
  all.found.sort((a, b) => a.distance_m - b.distance_m);
  const markers = all.found.slice(0, MAX_MARKERS).map((p) => ({ lat: p.latitude, lon: p.longitude, label: p.name.slice(0, 120), detail: p.address.slice(0, 400), ...(p.id ? { place: p.id } : {}) }));
  show(t, hostComponent("Map"), all.here ? { markers, here: all.here } : { markers });
}

/** The places Apple Maps finds for `query` (what a person would type into Maps) near a point: where the Mac is
 *  (location()) unless `near` says, within `radius_m`, nearest first: names, addresses, phones, their own pages,
 *  coordinates. It draws them on a Map under the calling cell (`map: false` for the rows alone): asking is showing. */
export function places(query: string, opts: { near?: { latitude: number; longitude: number } | [number, number]; radius_m?: number; limit?: number; map?: boolean } = {}): Promise<Table<Near>> {
  return effect("net", `Finding ${query} nearby`, `Found ${query} nearby`, "Apple Maps", async () => {
    const h = await host();
    const n = opts.near;
    const at = Array.isArray(n) ? { latitude: n[0], longitude: n[1] } : n ?? await h.location();
    const found = await h.places(query, at, opts.radius_m ?? 5_000, opts.limit ?? 20);
    const rows = found
      .map((p): Near => ({ ...p, distance_m: Math.round(metres(at, p)), maps: `https://maps.apple.com/?q=${encodeURIComponent(p.name)}&ll=${p.latitude},${p.longitude}` }))
      .sort((a, b) => a.distance_m - b.distance_m);
    if (opts.map !== false) mapOf(rows, n ? undefined : [at.latitude, at.longitude]);
    return table(rows);
  }, (t) => ({ detail: `${t.length} place${t.length === 1 ? "" : "s"}` }));
}

/** Where the Mac is now: macOS's Core Location, asked by the harness browser's host (the one signed app harness has, so
 *  the one macOS can ask the person about; the first call makes it ask). Nothing else is tried: with Location Services
 *  off or refused it throws, saying which. */
export function location(): Promise<Fix & { at: string; maps: string }> {
  return effect("exec", "Finding where this Mac is", "Found where this Mac is", "macOS Location Services", async () => {
    const f = await (await host()).location();
    return { ...f, at: new Date(f.at_ms).toISOString(), maps: `https://www.google.com/maps?q=${f.latitude},${f.longitude}` };
  }, (f) => ({ detail: `${f.latitude.toFixed(4)}, ${f.longitude.toFixed(4)} (within ${Math.round(f.accuracy_m)} m)` }));
}

