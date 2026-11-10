// `browser.*`: the harness browser (embedded Chromium the daemon runs, crates/harness/harness-browser) as a cell uses it
// directly. googleSearch, gmail and withHiddenPage drive it too; these are for a page the person should see, and for
// what the browser holds.
import { host, newPage, withHiddenPage, type Extension, type Fix, type Page, type Place, type ViewInfo } from "../cdp.ts";
import { pmap } from "../pmap.ts";
import { table, type Table } from "../table.ts";
import { effect } from "../effects.ts";

const hostOf = (url: string) => { try { return new URL(url).host || url; } catch { return url; } };

/** Open `url` in a view of its own that stays open (in the browser pane's tabs, after this kernel too), loaded or after
 *  20 s still loading; the app shows it in the browser pane beside the chat. The Page drives it (goto, evaluate, send...). */
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

/** The places Apple Maps finds for `query` (what a person would type into Maps) near a point: where the Mac is
 *  (location()) unless `near` says, within `radius_m`, nearest first. One call, under a second: names, addresses,
 *  phones, their own pages, coordinates (a Map's markers as they are). */
export function places(query: string, opts: { near?: { latitude: number; longitude: number } | [number, number]; radius_m?: number; limit?: number } = {}): Promise<Table<Near>> {
  return effect("net", `Finding ${query} nearby`, `Found ${query} nearby`, "Apple Maps", async () => {
    const h = await host();
    const n = opts.near;
    const at = Array.isArray(n) ? { latitude: n[0], longitude: n[1] } : n ?? await h.location();
    const found = await h.places(query, at, opts.radius_m ?? 5_000, opts.limit ?? 20);
    return table(found
      .map((p): Near => ({ ...p, distance_m: Math.round(metres(at, p)), maps: `https://maps.apple.com/?q=${encodeURIComponent(p.name)}&ll=${p.latitude},${p.longitude}` }))
      .sort((a, b) => a.distance_m - b.distance_m));
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

