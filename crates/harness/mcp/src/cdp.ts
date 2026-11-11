// The kernel's browser: the harness browser (embedded Chromium, crates/harness/harness-browser), which this kernel's
// daemon runs. Nothing else: no Dia, no port on localhost, no search for a browser that happens to listen.
//
// How it is reached. `{op: "browser"}` to the daemon (daemon.ts) starts the host if it is not running and answers its
// socket, `$HARNESS_HOME/browser/host.sock`. On it go harness frames (`[type u8][len u32 BE][payload]`, wire.ts), each
// payload one deterministic-CBOR message (cbor.ts, the same encoder the daemon socket uses) of harness-proto's
// browser.rs: `KernelUp` from here (hello first, then open / attach / cdp / close / views / extensions / manage),
// `KernelDown` back. A view is one off-screen page; its DevTools traffic rides inside as `cdp { view, message }`, the
// page target's own CDP JSON, so a `Page` here is the same flat-session CDP client as before, over this transport.
//
// When the host is not available (its bundle missing, CEF failing, the daemon down) every call throws CdpUnavailable
// naming the harness browser and the daemon's error. Nothing retries another browser.
//
// The one other transport is `external(endpoint)`: a DevTools websocket some other browser serves. Only the gmail
// helpers use it, and only when asked by name (gmail.via({ cdp }) or HARNESS_GMAIL_CDP).
import { decode, encode } from "./cbor.ts";
import { req } from "./daemon.ts";
import { effect } from "./effects.ts";
import { REPL_ID } from "./repl.ts";
import { SHOWN, type Shown } from "./shown.ts";
import { currentTask } from "./tasks.ts";
import { SCHEMA, T_CBOR, frame, split } from "./wire.ts";

/** What this file is, in every error. */
export const WHAT = "harness browser (embedded Chromium)";

/** The harness browser cannot be reached. Its message names why; `brief` is what the app says (shown.ts `brief`). */
export class CdpUnavailable extends Error { override name = "CdpUnavailable"; readonly brief = "The harness browser is not available"; }

/** One view as the host has it (browser.rs `ViewInfo`). */
export type ViewInfo = {
  view: number; target: string; url: string; title: string; loading: boolean; can_back: boolean; can_forward: boolean;
  owner?: string; size: [number, number]; scale: number;
};
/** An extension installed in the profile (browser.rs `Extension`). */
/** Where the Mac is, as macOS's Core Location said it (browser.rs `KernelDown::Location`): degrees, metres (the radius
 *  it is sure of; altitude when it has one), and when it was measured (Unix ms). */
export type Fix = { latitude: number; longitude: number; accuracy_m: number; altitude_m?: number; at_ms: number };
/** One place Apple Maps found (browser.rs `Place`): empty strings where Maps has nothing. */
export type Place = { id: string; name: string; latitude: number; longitude: number; address: string; phone: string; url: string; category: string };
export type Extension = { id: string; name: string; version: string; enabled: boolean; popup?: string; source: string; path: string };

/** A socket client's message (browser.rs `KernelUp`). */
type KernelUp =
  | { t: "hello"; schema: number }
  | { t: "open"; req: number; url: string; owner?: string; size?: [number, number]; keep?: boolean }
  | { t: "attach"; req: number; view: number }
  | { t: "cdp"; view: number; message: string }
  | { t: "close"; view: number }
  | { t: "views"; req: number }
  | { t: "extensions"; req: number }
  | { t: "location"; req: number }
  | { t: "places"; req: number; query: string; latitude: number; longitude: number; radius_m: number; limit: number }
  | { t: "manage"; req: number; url?: string };
/** The host's message (browser.rs `KernelDown`). */
type KernelDown =
  | { t: "hello"; schema: number; pid: number }
  | { t: "opened"; req: number; view: ViewInfo }
  | { t: "cdp"; view: number; message: string }
  | { t: "closed"; view: number; why: string }
  | { t: "views"; req: number; views: ViewInfo[] }
  | { t: "extensions"; req: number; extensions: Extension[] }
  | ({ t: "location"; req: number } & Fix)
  | { t: "places"; req: number; places: Place[] }
  | { t: "done"; req: number }
  | { t: "error"; req?: number | null; error: string };

/** The bounds on each wait (rule bounded-progress): the hello, a host request, one CDP call. */
const HELLO_MS = 5_000;
const HOST_MS = 30_000;
/** A location may wait on the person: the first one makes macOS ask them to let Harness Browser use Location Services.
 *  The host gives up after 60 s (location.rs `WAIT`) and says why; this bound is past it. */
const LOCATION_MS = 70_000;
const CDP_MS = 120_000;

/** A CDP message (an answer when it has an id, else an event). */
type CdpMsg = { id?: number; method?: string; params?: any; result?: any; error?: { message: string; data?: string }; sessionId?: string };
type Listener = (params: any) => void;

/** One page's DevTools session, whatever carries it. */
export interface CdpSession {
  readonly closed: boolean;
  /** Why it closed, once it has. */
  readonly why: string | undefined;
  send<T = any>(method: string, params?: object): Promise<T>;
  on(method: string, fn: Listener): () => void;
  close(): Promise<void>;
}

/** A promise that rejects after `ms` with `what` named, and the clearing of its timer. */
function bounded<T>(p: Promise<T>, ms: number, what: string): Promise<T> {
  let timer: Timer | undefined;
  const t = new Promise<never>((_, reject) => { timer = setTimeout(() => reject(new Error(`${what}: no answer within ${ms / 1000} s`)), ms); });
  return Promise.race([p, t]).finally(() => clearTimeout(timer));
}

/** Events and pending calls of one session, by method and by id. */
class Calls {
  #pending = new Map<number, { resolve: (v: any) => void; reject: (e: Error) => void; method: string }>();
  #listeners = new Set<{ method: string; fn: Listener }>();
  add(id: number, method: string): Promise<any> {
    return new Promise((resolve, reject) => this.#pending.set(id, { resolve, reject, method }));
  }
  /** `m` arrived for this session: its call settled, or its event heard. */
  take(m: CdpMsg, where: string) {
    if (m.id !== undefined) {
      const p = this.#pending.get(m.id);
      if (!p) return;
      this.#pending.delete(m.id);
      m.error ? p.reject(new Error(`${p.method} on ${where}: ${m.error.message}${m.error.data ? ` (${m.error.data})` : ""}`)) : p.resolve(m.result);
      return;
    }
    for (const l of this.#listeners) if (l.method === m.method) l.fn(m.params);
  }
  on(method: string, fn: Listener): () => void {
    const l = { method, fn };
    this.#listeners.add(l);
    return () => { this.#listeners.delete(l); };
  }
  drop(id: number) { this.#pending.delete(id); }
  fail(why: string) {
    for (const p of this.#pending.values()) p.reject(new Error(`${p.method}: ${why}`));
    this.#pending.clear();
  }
}

/** One connection to the host's socket. */
export class Host {
  #socket!: { write(b: Uint8Array): number; end(): void };
  #out: Uint8Array[] = [];
  #req = 0;
  #cdpId = 0;
  #requests = new Map<number, { resolve: (m: KernelDown) => void; reject: (e: Error) => void; what: string }>();
  #views = new Map<number, HostView>();
  #hello!: { resolve: (pid: number) => void; reject: (e: Error) => void };
  closed = false;
  why: string | undefined;
  /** The host's pid, from its hello. */
  pid = 0;

  private constructor(readonly sock: string) {}

  /** Ask the daemon for the host (starting it), connect to its socket and say hello. */
  static async connect(): Promise<Host> {
    let where: { pid: number; sock: string; service: string };
    try {
      where = await req<{ pid: number; sock: string; service: string }>({ op: "browser" });
    } catch (e) {
      throw new CdpUnavailable(`${WHAT} is not available: the daemon could not start it: ${(e as Error).message}`);
    }
    const h = new Host(where.sock);
    const greeted = new Promise<number>((resolve, reject) => { h.#hello = { resolve, reject }; });
    greeted.catch(() => {}); // awaited below, once connected; a connect that fails first throws its own error
    let buf = new Uint8Array(0);
    try {
      await Bun.connect({
        unix: where.sock,
        socket: {
          open(s) { h.#socket = s; h.#flush(); },
          drain() { h.#flush(); },
          data(_s, chunk) {
            const next = new Uint8Array(buf.length + chunk.length);
            next.set(buf); next.set(chunk, buf.length); buf = next;
            for (;;) {
              let f;
              try { f = split(buf); } catch (e) { return h.#lost(`the host sent a bad frame: ${(e as Error).message}`); }
              if (!f) return;
              buf = buf.subarray(f.size);
              if (f.t !== T_CBOR) return h.#lost(`the host sent a type ${f.t} frame (its messages are type ${T_CBOR})`);
              let m: KernelDown;
              try { m = decode(f.payload) as KernelDown; } catch (e) { return h.#lost(`the host sent a message that is not CBOR: ${(e as Error).message}`); }
              h.#onMessage(m);
            }
          },
          close() { h.#lost("the host closed the connection"); },
          error(_s, e) { h.#lost(`the connection failed: ${e.message}`); },
          connectError(_s, e) { h.#lost(`connecting to ${where.sock}: ${e.message}`); },
        },
      });
    } catch (e) {
      throw new CdpUnavailable(`${WHAT}: the daemon says its host (pid ${where.pid}) listens at ${where.sock}, but connecting failed: ${(e as Error).message}`);
    }
    h.#write({ t: "hello", schema: SCHEMA });
    try {
      h.pid = await bounded(greeted, HELLO_MS, `${WHAT} at ${where.sock}: the hello`);
    } catch (e) {
      h.close();
      throw new CdpUnavailable(`${WHAT}: ${(e as Error).message}`);
    }
    return h;
  }

  #flush() {
    if (!this.#socket) return;
    while (this.#out.length) {
      const b = this.#out[0]!;
      const n = Math.max(0, this.#socket.write(b));
      if (n < b.length) { this.#out[0] = b.subarray(n); return; }
      this.#out.shift();
    }
  }

  #write(m: KernelUp) {
    if (this.closed) throw new CdpUnavailable(`${WHAT}: the connection to ${this.sock} is closed (${this.why})`);
    this.#out.push(frame(T_CBOR, encode(m)));
    this.#flush();
  }

  #lost(why: string) {
    if (this.closed) return;
    this.closed = true;
    this.why = why;
    this.#hello?.reject(new Error(why));
    for (const r of this.#requests.values()) r.reject(new CdpUnavailable(`${WHAT}: ${r.what}: ${why}`));
    this.#requests.clear();
    for (const v of this.#views.values()) v.gone(why);
    this.#views.clear();
    try { this.#socket?.end(); } catch {}
  }

  #onMessage(m: KernelDown) {
    switch (m.t) {
      case "hello":
        if (m.schema !== SCHEMA) return this.#lost(`the host speaks harness schema ${m.schema}, this kernel ${SCHEMA}: rebuild and restart one of them`);
        this.pid = m.pid;
        return this.#hello.resolve(m.pid);
      case "cdp": {
        const v = this.#views.get(m.view);
        if (!v) return;
        let msg: CdpMsg;
        try { msg = JSON.parse(m.message) as CdpMsg; } catch (e) { return console.error(`[harness browser] view ${m.view} sent CDP that is not JSON: ${(e as Error).message}`); }
        return v.take(msg);
      }
      case "closed": {
        const v = this.#views.get(m.view);
        this.#views.delete(m.view);
        return v?.gone(`view ${m.view} closed: ${m.why}`);
      }
      case "error":
        if (m.req === undefined || m.req === null) {
          // A refusal that names no request: the hello's (the host closes after it), or one about a message that has none.
          if (!this.pid) return this.#lost(m.error);
          return console.error(`[harness browser] the host refused a message: ${m.error}`);
        }
        return this.#settle(m.req, m);
      default:
        return this.#settle(m.req, m);
    }
  }

  #settle(id: number, m: KernelDown) {
    const r = this.#requests.get(id);
    if (!r) return;
    this.#requests.delete(id);
    m.t === "error" ? r.reject(new Error(`${WHAT}: ${r.what}: ${m.error}`)) : r.resolve(m);
  }

  /** One host request, answered by the message with its `req`. */
  #ask(what: string, m: (req: number) => KernelUp, ms = HOST_MS): Promise<KernelDown> {
    const id = ++this.#req;
    const p = new Promise<KernelDown>((resolve, reject) => this.#requests.set(id, { resolve, reject, what }));
    this.#write(m(id));
    return bounded(p, ms, `${WHAT}: ${what}`).finally(() => this.#requests.delete(id));
  }

  /** A new view at `url`: closed with this connection unless `keep`. */
  async open(url: string, opts: { owner?: string; size?: [number, number]; keep?: boolean } = {}): Promise<HostView> {
    const m = await this.#ask(`opening a view at ${url}`, (req) => ({ t: "open", req, url, owner: opts.owner, size: opts.size, keep: opts.keep || undefined }));
    if (m.t !== "opened") throw new Error(`${WHAT}: opening ${url} was answered with a \`${m.t}\` message`);
    return this.#view(m.view);
  }

  /** A view someone else opened (a tab the person opened in the app), driven from here too. */
  async attach(view: number): Promise<HostView> {
    const m = await this.#ask(`attaching to view ${view}`, (req) => ({ t: "attach", req, view }));
    if (m.t !== "opened") throw new Error(`${WHAT}: attaching to view ${view} was answered with a \`${m.t}\` message`);
    return this.#view(m.view);
  }

  #view(info: ViewInfo): HostView {
    const v = new HostView(this, info, async (method, params) => {
      const id = ++this.#cdpId;
      // Written first: a closed connection throws before anything waits on an answer.
      this.#write({ t: "cdp", view: info.view, message: JSON.stringify({ id, method, params }) });
      return bounded(v.calls.add(id, method), CDP_MS, `${method} on view ${info.view}`).finally(() => v.calls.drop(id));
    }, () => { this.#views.delete(info.view); if (!this.closed) this.#write({ t: "close", view: info.view }); });
    this.#views.set(info.view, v);
    return v;
  }

  async views(): Promise<ViewInfo[]> {
    const m = await this.#ask("listing the views", (req) => ({ t: "views", req }));
    if (m.t !== "views") throw new Error(`${WHAT}: listing the views was answered with a \`${m.t}\` message`);
    return m.views;
  }

  async extensions(): Promise<Extension[]> {
    const m = await this.#ask("listing the extensions", (req) => ({ t: "extensions", req }));
    if (m.t !== "extensions") throw new Error(`${WHAT}: listing the extensions was answered with a \`${m.t}\` message`);
    return m.extensions;
  }

  /** Where the Mac is now, from macOS's Core Location (the host asks it; the first time macOS asks the person). */
  async location(): Promise<Fix> {
    const m = await this.#ask("asking macOS where this Mac is", (req) => ({ t: "location", req }), LOCATION_MS);
    if (m.t !== "location") throw new Error(`${WHAT}: asking where this Mac is was answered with a \`${m.t}\` message`);
    return { latitude: m.latitude, longitude: m.longitude, accuracy_m: m.accuracy_m, altitude_m: m.altitude_m, at_ms: m.at_ms };
  }

  /** What Apple Maps finds for `query` within `radius_m` metres of a point, as Maps ranks it (the host asks MapKit). */
  async places(query: string, at: { latitude: number; longitude: number }, radius_m: number, limit: number): Promise<Place[]> {
    const m = await this.#ask(`asking Apple Maps for ${query}`, (req) => ({ t: "places", req, query, latitude: at.latitude, longitude: at.longitude, radius_m, limit }));
    if (m.t !== "places") throw new Error(`${WHAT}: asking Apple Maps for ${query} was answered with a \`${m.t}\` message`);
    return m.places;
  }

  /** Open the manage window (a Chrome-style window of the host's own) at `url`, chrome://extensions when none. */
  async manage(url?: string): Promise<void> {
    await this.#ask(`opening the manage window${url ? ` at ${url}` : ""}`, (req) => ({ t: "manage", req, url }));
  }

  close() { this.#lost("closed by the kernel"); }
}

/** One view of the host, as a CDP session. */
export class HostView implements CdpSession {
  readonly calls = new Calls();
  closed = false;
  why: string | undefined;
  readonly #send: (method: string, params: object) => Promise<any>;
  readonly #close: () => void;
  constructor(readonly host: Host, readonly info: ViewInfo, send: (method: string, params: object) => Promise<any>, close: () => void) {
    this.#send = send;
    this.#close = close;
  }
  send<T = any>(method: string, params: object = {}): Promise<T> {
    if (this.closed) return Promise.reject(new Error(`${method} on view ${this.info.view}: it is closed (${this.why})`));
    return this.#send(method, params);
  }
  on(method: string, fn: Listener) { return this.calls.on(method, fn); }
  take(m: CdpMsg) { this.calls.take(m, `view ${this.info.view}`); }
  gone(why: string) {
    if (this.closed) return;
    this.closed = true;
    this.why = why;
    this.calls.fail(why);
  }
  async close() {
    if (this.closed) return;
    this.gone("closed by the kernel");
    this.#close();
  }
}

/** A page: one CDP session with the helpers cells use. `view` is the harness browser's id for it (a string, as
 *  `<Browser view={page.view} />` takes it); null for a page of an external endpoint. */
export class Page {
  #url: string;
  #title: string;
  constructor(readonly session: CdpSession, readonly view: string | null, readonly target: string, url = "about:blank", title = "") {
    this.#url = url;
    this.#title = title;
    session.on("Page.frameNavigated", (p) => { if (!p?.frame?.parentId && typeof p?.frame?.url === "string") this.#url = p.frame.url; });
  }

  get closed() { return this.session.closed; }

  send<T = any>(method: string, params: object = {}) { return this.session.send<T>(method, params); }

  /** Hear CDP event `method` of this page; returns the unsubscribe. */
  on(method: string, fn: Listener) { return this.session.on(method, fn); }

  waitFor(method: string, timeoutMs: number): Promise<any> {
    return new Promise((resolve, reject) => {
      const off = this.on(method, (p) => { clearTimeout(t); off(); resolve(p); });
      const t = setTimeout(() => { off(); reject(new Error(`timed out after ${timeoutMs}ms waiting for ${method} on ${this.#where()}`)); }, timeoutMs);
    });
  }

  #where() { return this.view === null ? `target ${this.target}` : `view ${this.view}`; }

  /** Load `url` and wait for its load event. */
  goto(url: string, timeoutMs = 20_000): Promise<void> {
    let host = url;
    try { host = new URL(url).host || url; } catch {}
    return effect("net", `Opening ${host}`, `Opened ${host}`, url, () => this.#goto(url, timeoutMs), () => ({ detail: this.#title, show: this }));
  }

  async #goto(url: string, timeoutMs: number) {
    // Both waits are settled here and nowhere else: a wait left to reject on its own timer is an unhandled rejection,
    // which ends the kernel (and with it the agent's REPL and everything it held).
    const loaded = this.waitFor("Page.loadEventFired", timeoutMs);
    loaded.catch(() => {});
    const went = this.send<{ errorText?: string }>("Page.navigate", { url });
    went.catch(() => {});
    let r: { errorText?: string };
    try {
      // A host that never answers holds Page.navigate for Chromium's own connect timeout (over a minute): the page's
      // bound is `timeoutMs`, for the whole of getting there.
      r = await bounded(went, timeoutMs, `navigate ${url} (${this.#where()})`);
    } catch (e) {
      await this.send("Page.stopLoading").catch(() => {});
      throw e;
    }
    if (r.errorText) throw new Error(`navigate ${url} (${this.#where()}): ${r.errorText}`);
    await loaded;
    await this.refresh();
  }

  /** Read the page's url and title again (what its card shows). */
  async refresh() {
    const at = await this.evaluate(() => ({ url: location.href, title: document.title })).catch(() => undefined);
    if (at) { this.#url = at.url; this.#title = at.title; }
  }

  /** Run a function in the page, or a string as an expression there (`'document.title'`). A function is serialized with
   *  toString, so it cannot close over local variables; pass `arg`. */
  async evaluate<T, A = undefined>(fn: ((arg: A) => T | Promise<T>) | string, arg?: A): Promise<T> {
    const expression = typeof fn === "string" ? fn : `(${fn.toString()})(${JSON.stringify(arg ?? null)})`;
    const r = await this.send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) throw new Error(`evaluate on ${this.#where()}: ${r.exceptionDetails.exception?.description ?? r.exceptionDetails.text}`);
    return r.result.value as T;
  }

  async waitForSelector(selector: string, timeoutMs = 10_000): Promise<boolean> {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (await this.evaluate((s) => !!document.querySelector(s), selector)) return true;
      await Bun.sleep(100);
    }
    return false;
  }

  /** The page's words as a person reads them (the body's innerText), once it has settled: loaded, and the same length
   *  on two looks 250 ms apart (a page that fills itself in after its load event, as most do), or as it is after
   *  `timeoutMs`. No sleep is needed before it. */
  async text(timeoutMs = 8_000): Promise<string> {
    const deadline = Date.now() + timeoutMs;
    let last = -1;
    while (Date.now() < deadline) {
      const now = await this.evaluate(() => (document.body && document.readyState === "complete" ? document.body.innerText.length : -1)).catch(() => -1);
      if (now > 0 && now === last) break;
      last = now;
      await Bun.sleep(250);
    }
    return this.evaluate(() => document.body?.innerText ?? "");
  }

  url() { return this.evaluate(() => location.href); }

  title() { return this.evaluate(() => document.title); }

  close() { return this.session.close().catch(() => {}); }

  /** Opened for the person (browser.open: a view that stays): its step in the chat is a line they press to see it in
   *  the browser pane, where they can scroll, click and type in it. Set by newPage. */
  kept = false;

  /** The card the app draws for it (shown.ts); `pane` when it is the person's to use ([`Page.kept`]). */
  [SHOWN](): Shown {
    return { t: "browser", view: this.view ?? "", url: this.#url, title: this.#title, ...(this.kept ? { pane: true as const } : {}) };
  }
}

let shared: Promise<Host> | undefined;
/** This kernel's connection to the harness browser, made once and again after it drops. */
export async function host(): Promise<Host> {
  if (shared) {
    const h = await shared.catch(() => undefined);
    if (h && !h.closed) return h;
  }
  shared = Host.connect();
  try {
    return await shared;
  } catch (e) {
    shared = undefined;
    throw e;
  }
}

/** Who opened a view: the running cell (`<repl>/<task>`), else this kernel. */
const owner = () => `${REPL_ID}/${currentTask()?.id ?? "kernel"}`;

/** A page of the harness browser, its Page domain on (never Runtime: evaluate needs no Runtime.enable, nothing here
 *  hears its events, and a page can tell it is on, which is how sites pick out an automated browser); at `url`,
 *  loaded, when given. `keep`: it outlives this kernel's connection (a page opened for the person). `size`: its view's
 *  size in points until the app draws it (the app sizes a view to the box it is drawn in). */
export async function newPage(opts: { url?: string; keep?: boolean; size?: [number, number] } = {}): Promise<Page> {
  // Always opened blank: the page is told what browser it is (asChrome) before its first request leaves.
  const v = await (await host()).open("about:blank", { owner: owner(), keep: opts.keep, size: opts.size });
  const page = new Page(v, String(v.info.view), v.info.target, v.info.url, v.info.title);
  page.kept = !!opts.keep;
  try {
    await Promise.all([page.send("Page.enable"), asChrome(page)]);
    if (opts.url) {
      const went = page.send<{ errorText?: string }>("Page.navigate", { url: opts.url });
      went.catch(() => {});
      const r = await bounded(went, 20_000, `navigate ${opts.url} (view ${page.view})`);
      if (r.errorText) throw new Error(`navigate ${opts.url} (view ${page.view}): ${r.errorText}`);
      await waitLoaded(page, opts.url, 20_000);
    }
  } catch (e) {
    if (!opts.keep) await page.close();
    throw e;
  }
  return page;
}

/** What the harness browser is, said the way Chrome says it: the embedded Chromium is Chrome's engine at Chrome's
 *  version, but an embedder's build names only "Chromium" in its client hints (`Sec-CH-UA`, `navigator.userAgentData`)
 *  while its User-Agent header says Chrome, a mismatch sites read as an automated browser. This adds the "Google
 *  Chrome" brand at the same version, beside the brands the build already sends. Read once a kernel. */
let chrome: Promise<object> | undefined;
function asChrome(page: Page): Promise<unknown> {
  chrome ??= (async () => {
    const v = await page.send<{ product: string; userAgent: string }>("Browser.getVersion");
    const full = /\/([\d.]+)/.exec(v.product)?.[1];
    if (!full) throw new Error(`${WHAT}: Browser.getVersion's product is \`${v.product}\`, not Chrome/<version>`);
    const major = full.split(".")[0]!;
    const os = Bun.spawnSync(["/usr/bin/sw_vers", "-productVersion"]).stdout.toString().trim();
    if (!/^\d+(\.\d+)*$/.test(os)) throw new Error(`${WHAT}: sw_vers -productVersion printed \`${os}\`, not a macOS version`);
    const brands = (ver: string, grease: string) => [{ brand: "Chromium", version: ver }, { brand: "Google Chrome", version: ver }, { brand: "Not:A-Brand", version: grease }];
    return {
      userAgent: v.userAgent,
      userAgentMetadata: {
        brands: brands(major, "8"), fullVersionList: brands(full, "8.0.0.0"), fullVersion: full,
        platform: "macOS", platformVersion: os.split(".").concat("0", "0").slice(0, 3).join("."),
        architecture: process.arch === "arm64" ? "arm" : "x86", bitness: "64", model: "", mobile: false, wow64: false,
      },
    };
  })();
  chrome.catch(() => { chrome = undefined; });
  return chrome.then((o) => page.send("Emulation.setUserAgentOverride", o));
}

/** Wait (at most `ms`) for a page opened at a url to finish loading; a page still loading then is returned as it is.
 *  A new view starts on about:blank, already `complete`, before its navigation commits: loaded means that document has
 *  been replaced (by the page, a redirect or an error page) and the new one is complete. */
async function waitLoaded(page: Page, url: string, ms: number) {
  const deadline = Date.now() + ms;
  const blank = url === "about:blank";
  while (Date.now() < deadline) {
    const done = await page.evaluate((b) => document.readyState === "complete" && (b || location.href !== "about:blank"), blank).catch(() => false);
    if (done) break;
    await Bun.sleep(100);
  }
  await page.refresh();
}

/** A page of the harness browser for `fn`, closed afterwards. */
export async function withHiddenPage<T>(fn: (page: Page) => Promise<T>): Promise<T> {
  const page = await newPage();
  try { return await fn(page); } finally { await page.close(); }
}

// ---- an external DevTools endpoint (explicit opt-in only: gmail.via)

/** A browser-level DevTools websocket at `endpoint` (`http://127.0.0.1:9222`), its pages hidden targets on flat sessions. */
export class External {
  #ws: WebSocket;
  #id = 0;
  #sessions = new Map<string, Calls>();
  #browser = new Calls();
  closed = false;
  why: string | undefined;

  private constructor(readonly endpoint: string, ws: WebSocket) {
    this.#ws = ws;
    ws.addEventListener("message", (e) => {
      let m: CdpMsg;
      try { m = JSON.parse(String(e.data)) as CdpMsg; } catch { return; }
      (m.sessionId ? this.#sessions.get(m.sessionId) : this.#browser)?.take(m, `${endpoint}${m.sessionId ? ` session ${m.sessionId}` : ""}`);
    });
    ws.addEventListener("close", () => {
      this.closed = true;
      this.why = `the DevTools websocket at ${endpoint} closed`;
      this.#browser.fail(this.why);
      for (const s of this.#sessions.values()) s.fail(this.why);
    });
  }

  static async connect(endpoint: string): Promise<External> {
    const res = await fetch(`${endpoint}/json/version`, { signal: AbortSignal.timeout(3_000) }).catch((e) => {
      throw new CdpUnavailable(`no DevTools endpoint at ${endpoint} (asked for by gmail.via or HARNESS_GMAIL_CDP): ${(e as Error).message}`);
    });
    const { webSocketDebuggerUrl } = (await res.json()) as { webSocketDebuggerUrl: string };
    const ws = new WebSocket(webSocketDebuggerUrl);
    await bounded(new Promise<void>((resolve, reject) => {
      ws.addEventListener("open", () => resolve(), { once: true });
      ws.addEventListener("error", () => reject(new CdpUnavailable(`the DevTools websocket ${webSocketDebuggerUrl} (from ${endpoint}) failed to open`)), { once: true });
    }), HELLO_MS, `the DevTools websocket ${webSocketDebuggerUrl}`);
    return new External(endpoint, ws);
  }

  #send<T = any>(calls: Calls, method: string, params: object, sessionId?: string): Promise<T> {
    if (this.closed) return Promise.reject(new Error(`${method}: ${this.why}`));
    const id = ++this.#id;
    this.#ws.send(JSON.stringify({ id, method, params, sessionId }));
    return bounded(calls.add(id, method), CDP_MS, `${method} at ${this.endpoint}`).finally(() => calls.drop(id));
  }

  /** A hidden target (not in the tab strip; closed when this connection drops). */
  async newPage(): Promise<Page> {
    const { targetId } = await this.#send<{ targetId: string }>(this.#browser, "Target.createTarget", { url: "about:blank", hidden: true, background: true });
    const { sessionId } = await this.#send<{ sessionId: string }>(this.#browser, "Target.attachToTarget", { targetId, flatten: true });
    const calls = new Calls();
    this.#sessions.set(sessionId, calls);
    const ext = this;
    const session: CdpSession = {
      get closed() { return ext.closed; },
      get why() { return ext.why; },
      send: (method, params = {}) => ext.#send(calls, method, params, sessionId),
      on: (method, fn) => calls.on(method, fn),
      close: async () => {
        ext.#sessions.delete(sessionId);
        await ext.#send(ext.#browser, "Target.closeTarget", { targetId }).catch(() => {});
      },
    };
    const page = new Page(session, null, targetId);
    await page.send("Page.enable");
    return page;
  }

  close() { this.#ws.close(); }
}

const externals = new Map<string, Promise<External>>();
/** The connection to external DevTools endpoint `endpoint`, made once and again after it drops. */
export async function external(endpoint: string): Promise<External> {
  const had = externals.get(endpoint);
  if (had) {
    const e = await had.catch(() => undefined);
    if (e && !e.closed) return e;
  }
  const p = External.connect(endpoint);
  externals.set(endpoint, p);
  try {
    return await p;
  } catch (e) {
    externals.delete(endpoint);
    throw e;
  }
}
