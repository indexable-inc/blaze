// `browser.*`: the harness browser (embedded Chromium the daemon runs, crates/harness/harness-browser) as a cell uses it
// directly. googleSearch, gmail and withHiddenPage drive it too; these are for a page the person should see, and for
// what the browser holds.
import { host, newPage, type Extension, type Page, type ViewInfo } from "../cdp.ts";
import { table, type Table } from "../table.ts";
import { effect } from "../effects.ts";

const hostOf = (url: string) => { try { return new URL(url).host || url; } catch { return url; } };

/** Open `url` in a view of its own that stays open (in the browser pane's tabs, after this kernel too), loaded or after
 *  20 s still loading; the cell's step shows it as a live card. The Page drives it (goto, evaluate, send...). */
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

export const browser = { open, views, extensions, manage };
