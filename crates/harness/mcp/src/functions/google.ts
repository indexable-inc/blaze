import { withHiddenPage, type Page } from "../cdp.ts";
import { table, type Table } from "../table.ts";
import { effect, oneLine } from "../effects.ts";

export type SearchResult = {
  title: string;
  /** Real destination when Google let us resolve it, otherwise Google's /goto link (opening it lands on the page). */
  url: string;
  /** false when `url` is still the /goto link: Google throttles resolving them after ~2 per results page. */
  resolved: boolean;
  /** Breadcrumb Google displays, e.g. "https://www.sqlite.org › wal". */
  site: string;
  snippet: string;
};

/** Search Google in a page of the harness browser (embedded Chromium, cdp.ts) and return the organic results. */
export function googleSearch(query: string, opts: { limit?: number; hl?: string } = {}): Promise<Table<SearchResult>> {
  const q = JSON.stringify(oneLine(query, 60));
  return effect("search", `Searching Google for ${q}`, `Searched Google for ${q}`, query, () => search(query, opts), (r) => ({
    detail: `${r.length} result${r.length === 1 ? "" : "s"}`, show: r, kind: "search",
  }));
}

async function search(query: string, opts: { limit?: number; hl?: string }): Promise<Table<SearchResult>> {
  const { limit = 10, hl = "en" } = opts;
  return withHiddenPage(async (page) => {
    await page.goto(`https://www.google.com/search?${new URLSearchParams({ q: query, hl })}`);
    await page.waitForSelector("#search a h3, #rso a h3", 8_000);
    const current = await page.url();
    if (new URL(current).pathname.startsWith("/sorry")) throw new Error(`Google served a CAPTCHA (${current}); solve it once in the harness browser pane (cmd+shift+B) and retry`);
    const raw = (await page.evaluate(() => {
      const out: { title: string; href: string; cite: string; snippet: string }[] = [];
      const seen = new Set<string>();
      for (const h3 of document.querySelectorAll<HTMLElement>("#search a h3, #rso a h3")) {
        const a = h3.closest("a");
        if (!a || !a.href.startsWith("http") || seen.has(a.href)) continue;
        const u = new URL(a.href);
        // Results are wrapped in encrypted /goto (or legacy /url) redirects; other google.com links are Google's own UI.
        if (u.hostname.endsWith("google.com") && u.pathname !== "/goto" && u.pathname !== "/url") continue;
        seen.add(a.href);
        const block = a.closest<HTMLElement>(".MjjYud, .g, [data-hveid]") ?? a.parentElement!;
        const snippet = block.querySelector<HTMLElement>(".VwiC3b, [data-sncf], [style*='-webkit-line-clamp']")?.innerText ?? "";
        const cite = block.querySelector<HTMLElement>("cite")?.innerText ?? "";
        out.push({ title: h3.innerText.trim(), href: a.href, cite: cite.trim(), snippet: snippet.trim() });
      }
      return out;
    })).slice(0, limit);
    const locations = await resolveInPage(page, raw.map((r) => r.href));
    return table(raw.map((r): SearchResult => {
      const direct = new URL(r.href).hostname.endsWith("google.com") ? locations.get(r.href) : r.href;
      return { title: r.title, url: direct ?? r.href, resolved: !!direct, site: r.cite, snippet: r.snippet };
    }));
  });
}

/** Fetch each /goto link from inside the Google page (the harness browser's cookies and UA) and read the 302 Location via CDP Network events. */
async function resolveInPage(page: Page, hrefs: string[]): Promise<Map<string, string>> {
  const targets = hrefs.filter((h) => new URL(h).hostname.endsWith("google.com"));
  const found = new Map<string, string>();
  if (!targets.length) return found;
  await page.send("Network.enable");
  const urlById = new Map<string, string>();
  const offs = [
    page.on("Network.requestWillBeSent", (e) => urlById.set(e.requestId, e.request.url)),
    page.on("Network.responseReceivedExtraInfo", (e) => {
      const url = urlById.get(e.requestId);
      const loc = e.headers?.location ?? e.headers?.Location;
      if (url && loc && e.statusCode >= 300 && e.statusCode < 400) found.set(url, loc);
    }),
  ];
  try {
    await page.evaluate(
      (hs) => Promise.all(hs.map((h) => fetch(h, { redirect: "manual", credentials: "include", signal: AbortSignal.timeout(4000) }).catch(() => {}))),
      targets,
    );
    await Bun.sleep(150); // ExtraInfo events can trail the fetch resolving
  } finally {
    offs.forEach((off) => off());
    await page.send("Network.disable").catch(() => {});
  }
  return found;
}
