// Minimal Chrome DevTools Protocol client: one browser-level websocket, flat sessions,
// hidden tabs (not in the tab strip; closed automatically when this connection drops).

type Msg = { id?: number; method?: string; params?: any; result?: any; error?: { message: string }; sessionId?: string };
type Listener = (params: any, sessionId?: string) => void;

export class Browser {
  #ws: WebSocket;
  #id = 0;
  #pending = new Map<number, { resolve: (v: any) => void; reject: (e: Error) => void; method: string }>();
  #listeners = new Set<{ method: string; sessionId?: string; fn: Listener }>();
  closed = false;

  private constructor(ws: WebSocket) {
    this.#ws = ws;
    ws.addEventListener("message", (e) => this.#onMessage(JSON.parse(String(e.data)) as Msg));
    ws.addEventListener("close", () => {
      this.closed = true;
      for (const p of this.#pending.values()) p.reject(new Error(`CDP connection closed during ${p.method}`));
      this.#pending.clear();
    });
  }

  static async connect(endpoint = process.env.CDP_ENDPOINT ?? "http://localhost:9222"): Promise<Browser> {
    const res = await fetch(`${endpoint}/json/version`).catch((e) => {
      throw new Error(`no Chrome DevTools at ${endpoint} (start Chrome with --remote-debugging-port=9222): ${e.message}`);
    });
    const { webSocketDebuggerUrl } = (await res.json()) as { webSocketDebuggerUrl: string };
    const ws = new WebSocket(webSocketDebuggerUrl);
    await new Promise<void>((resolve, reject) => {
      ws.addEventListener("open", () => resolve(), { once: true });
      ws.addEventListener("error", () => reject(new Error(`websocket to ${webSocketDebuggerUrl} failed`)), { once: true });
    });
    return new Browser(ws);
  }

  #onMessage(m: Msg) {
    if (m.id !== undefined) {
      const p = this.#pending.get(m.id);
      if (!p) return;
      this.#pending.delete(m.id);
      m.error ? p.reject(new Error(`${p.method}: ${m.error.message}`)) : p.resolve(m.result);
      return;
    }
    for (const l of this.#listeners) if (l.method === m.method && l.sessionId === m.sessionId) l.fn(m.params, m.sessionId);
  }

  send<T = any>(method: string, params: object = {}, sessionId?: string): Promise<T> {
    if (this.closed) return Promise.reject(new Error("CDP connection closed"));
    const id = ++this.#id;
    return new Promise<T>((resolve, reject) => {
      this.#pending.set(id, { resolve, reject, method });
      this.#ws.send(JSON.stringify({ id, method, params, sessionId }));
    });
  }

  on(method: string, sessionId: string | undefined, fn: Listener): () => void {
    const l = { method, sessionId, fn };
    this.#listeners.add(l);
    return () => this.#listeners.delete(l);
  }

  waitFor(method: string, sessionId: string | undefined, timeoutMs: number): Promise<any> {
    return new Promise((resolve, reject) => {
      const off = this.on(method, sessionId, (p) => { clearTimeout(t); off(); resolve(p); });
      const t = setTimeout(() => { off(); reject(new Error(`timed out after ${timeoutMs}ms waiting for ${method}`)); }, timeoutMs);
    });
  }

  async newHiddenPage(): Promise<Page> {
    const { targetId } = await this.send<{ targetId: string }>("Target.createTarget", { url: "about:blank", hidden: true, background: true });
    const { sessionId } = await this.send<{ sessionId: string }>("Target.attachToTarget", { targetId, flatten: true });
    await Promise.all([this.send("Page.enable", {}, sessionId), this.send("Runtime.enable", {}, sessionId)]);
    return new Page(this, targetId, sessionId);
  }

  close() { this.#ws.close(); }
}

export class Page {
  constructor(readonly browser: Browser, readonly targetId: string, readonly sessionId: string) {}

  send<T = any>(method: string, params: object = {}) { return this.browser.send<T>(method, params, this.sessionId); }

  async goto(url: string, timeoutMs = 20_000) {
    const loaded = this.browser.waitFor("Page.loadEventFired", this.sessionId, timeoutMs);
    const r = await this.send<{ errorText?: string }>("Page.navigate", { url });
    if (r.errorText) { loaded.catch(() => {}); throw new Error(`navigate ${url}: ${r.errorText}`); }
    await loaded;
  }

  /** Run a function in the page. It is serialized with toString, so it cannot close over local variables; pass `arg`. */
  async evaluate<T, A = undefined>(fn: (arg: A) => T | Promise<T>, arg?: A): Promise<T> {
    const expression = `(${fn.toString()})(${JSON.stringify(arg ?? null)})`;
    const r = await this.send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) throw new Error(`evaluate: ${r.exceptionDetails.exception?.description ?? r.exceptionDetails.text}`);
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

  url() { return this.evaluate(() => location.href); }

  close() { return this.browser.send("Target.closeTarget", { targetId: this.targetId }).catch(() => {}); }
}

let shared: Promise<Browser> | undefined;
/** One cached connection per process; reconnects if Chrome dropped it. */
export async function browser(): Promise<Browser> {
  if (shared) {
    const b = await shared.catch(() => undefined);
    if (b && !b.closed) return b;
  }
  shared = Browser.connect();
  return shared;
}

export async function withHiddenPage<T>(fn: (page: Page) => Promise<T>): Promise<T> {
  const page = await (await browser()).newHiddenPage();
  try { return await fn(page); } finally { await page.close(); }
}
