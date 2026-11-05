// Gmail through Gmail's own web client: a hidden mail.google.com tab in the browser on CDP (Dia), whose requests
// carry the logged-in session. No public Gmail API, no OAuth.
//
// How it works. The web client talks JSON arrays ("pt=ji") to /sync/u/<n>/i/<op>:
//   bv  "bulk view": a search; the request is the query plus paging, the reply the matching threads with a
//       summary of their messages.
//   fd  "fetch details": whole threads, every message with headers, its HTML body and attachment parts.
// Those requests need headers the client computes (an XSRF token, a client-info blob, a storage flag). We don't
// rebuild them: a script installed before the page loads records the X-* headers Gmail's own XHRs send to /sync/,
// and our requests reuse them from inside the page. The values stay in the page; nothing here reads, logs or returns
// them. Label names come from the boot data the page hands to `_GM_setData` (key hqyl8), recorded the same way.
// Attachments download from the legacy `/mail/u/<n>/?view=att` URL the fd reply names.
//
// The array positions below were read off live traffic (2026-11-05); when Gmail moves them, the error names the
// endpoint and what was missing.
import { mkdir } from "node:fs/promises";
import { join } from "node:path";
import { browser, type Page } from "../cdp.ts";
import { table, type Table } from "../table.ts";
import { effect, oneLine } from "../effects.ts";

const ACCOUNT = 0;
const ORIGIN = "https://mail.google.com";
const SYNC = `/sync/u/${ACCOUNT}/i`;
const HOME = () => process.env.HARNESS_HOME ?? `${process.env.HOME}/.harness`;

/** A thread as a search lists it. */
export type GmailThread = {
  id: string;
  subject: string;
  /** Sender of the newest message: display name, or the address when there is none. */
  from: string;
  fromEmail: string;
  date: Date;
  snippet: string;
  /** Readable label names: Inbox, Starred, Important, user labels... (internal ^ ids dropped). */
  labels: string[];
  unread: boolean;
  messages: number;
};

/** A message as a search lists it (no body; `read` it for that). */
export type GmailMessageRow = {
  id: string;
  threadId: string;
  subject: string;
  from: string;
  fromEmail: string;
  date: Date;
  snippet: string;
  labels: string[];
  unread: boolean;
};

export type GmailAttachment = { partId: string; name: string; mime: string; size: number };

export type GmailMessage = {
  id: string;
  threadId: string;
  subject: string;
  from: string;
  fromEmail: string;
  /** "Name <addr>" or "addr". */
  to: string[];
  cc: string[];
  bcc: string[];
  date: Date;
  labels: string[];
  unread: boolean;
  /** The RFC 822 Message-ID header, for replies. */
  messageId: string;
  /** Plain text of the body, with quoted history removed where it could be recognised. */
  text: string;
  /** true when quoted history was cut from `text`. */
  quotedRemoved: boolean;
  attachments: Table<GmailAttachment>;
};

export type GmailThreadDetail = { id: string; subject: string; labels: string[]; unread: boolean; messages: Table<GmailMessage> };

export type GmailLabel = {
  id: string;
  name: string;
  system: boolean;
  /** Search term selecting it, e.g. "in:inbox", "label:Receipts". */
  query: string;
};

export class GmailError extends Error { override name = "GmailError"; }

// ---------------------------------------------------------------- the page

// Runs in every document of the hidden tab before Gmail's own scripts.
const HOOK = `(() => {
  if (window.__blazeGmailHdrs) return;
  const H = {}; Object.defineProperty(window, "__blazeGmailHdrs", { value: H });
  const open = XMLHttpRequest.prototype.open, set = XMLHttpRequest.prototype.setRequestHeader;
  XMLHttpRequest.prototype.open = function (m, u) { this.__blazeUrl = String(u); return open.apply(this, arguments); };
  XMLHttpRequest.prototype.setRequestHeader = function (k, v) {
    if (this.__blazeUrl && this.__blazeUrl.includes("/sync/") && /^x-/i.test(k)) H[k] = v;
    return set.apply(this, arguments);
  };
  const B = {}; Object.defineProperty(window, "__blazeGmailBoot", { value: B });
  let real;
  Object.defineProperty(window, "_GM_setData", { configurable: true,
    get() { return real && function (d) { try { for (const k in d) B[k] = d[k]; } catch {} return real.apply(this, arguments); }; },
    set(f) { real = f; } });
})()`;

type Win = { __blazeGmailHdrs: Record<string, string>; __blazeGmailBoot: Record<string, unknown> };
const IDLE_MS = 5 * 60_000;
let shared: Promise<Page> | undefined;
let idleTimer: ReturnType<typeof setTimeout> | undefined;

function touch(p: Page) {
  clearTimeout(idleTimer);
  idleTimer = setTimeout(() => { if (shared) { shared = undefined; p.close(); } }, IDLE_MS);
  idleTimer.unref?.();
}

async function alive(p: Page): Promise<boolean> {
  if (p.browser.closed) return false;
  return p.evaluate(() => typeof (window as unknown as Win).__blazeGmailHdrs === "object").catch(() => false);
}

/** The hidden Gmail tab, opened once and kept for IDLE_MS after the last call. */
async function gmailPage(): Promise<Page> {
  if (shared) {
    const p = await shared.catch(() => undefined);
    if (p && (await alive(p))) { touch(p); return p; }
    shared = undefined;
  }
  shared = openPage();
  try {
    const p = await shared;
    touch(p);
    return p;
  } catch (e) {
    shared = undefined;
    throw e;
  }
}

async function openPage(): Promise<Page> {
  const p = await (await browser()).newHiddenPage();
  try {
    await p.send("Page.addScriptToEvaluateOnNewDocument", { source: HOOK });
    await p.goto(`${ORIGIN}/mail/u/${ACCOUNT}/#inbox`, 45_000);
    const at = new URL(await p.url());
    if (at.host !== "mail.google.com" || !at.pathname.startsWith("/mail/"))
      throw new GmailError(`Gmail is not logged in in the browser on CDP: mail.google.com/mail/u/${ACCOUNT}/ redirected to ${at.host}${at.pathname}. Sign in to Gmail in Dia and retry.`);
    // Gmail's first /sync/ request carries the headers we reuse; it makes one while booting. If it hasn't after a
    // few seconds, ask it for a search, which always goes to the server.
    const deadline = Date.now() + 20_000;
    let nudged = false;
    while (!(await p.evaluate(() => "X-Framework-Xsrf-Token" in (window as unknown as Win).__blazeGmailHdrs))) {
      if (Date.now() > deadline)
        throw new GmailError(`Gmail's web client made no /sync/ request with an X-Framework-Xsrf-Token header within 20s of loading mail.google.com/mail/u/${ACCOUNT}/; its request scheme changed, so gmail.* cannot reuse it`);
      if (!nudged && Date.now() > deadline - 14_000) { nudged = true; await p.evaluate(() => { location.hash = "#search/in%3Ainbox"; }); }
      await Bun.sleep(150);
    }
    return p;
  } catch (e) {
    await p.close();
    throw e;
  }
}

let counter = 0;
/** POST a JSON body to /sync/u/0/i/<op> from inside the Gmail page, with the headers Gmail's own client sent. */
async function sync(op: "bv" | "fd", body: unknown, retried = false): Promise<unknown[]> {
  const p = await gmailPage();
  const url = `${SYNC}/${op}?hl=en&c=${++counter}&rt=r&pt=ji`;
  const r = await p.evaluate(async ({ url, body }) => {
    const res = await fetch(url, {
      method: "POST", credentials: "include", body,
      headers: { ...(window as unknown as Win).__blazeGmailHdrs, "Content-Type": "application/json" },
    });
    return { status: res.status, text: await res.text() };
  }, { url, body: JSON.stringify(body) });
  if ((r.status === 401 || r.status === 403) && !retried) {
    // The session's token rotated; a fresh load gets a new one.
    const old = shared;
    shared = undefined;
    (await old?.catch(() => undefined))?.close();
    return sync(op, body, true);
  }
  if (r.status !== 200) throw new GmailError(`Gmail endpoint POST ${SYNC}/${op} returned HTTP ${r.status}: ${oneLine(r.text, 160)}`);
  try {
    const v = JSON.parse(r.text.replace(/^\)\]\}'\n?/, ""));
    if (!Array.isArray(v)) throw new Error("not an array");
    return v;
  } catch (e) {
    throw new GmailError(`Gmail endpoint POST ${SYNC}/${op} returned a reply that is not a JSON array (${(e as Error).message}): ${oneLine(r.text, 120)}`);
  }
}

// ---------------------------------------------------------------- decoding

type A = unknown[];
const arr = (v: unknown): A => (Array.isArray(v) ? v : []);
const str = (v: unknown): string => (typeof v === "string" ? v : "");
const num = (v: unknown): number => (typeof v === "number" ? v : 0);

const SYSTEM_LABELS: Record<string, [name: string, query: string]> = {
  "^i": ["Inbox", "in:inbox"],
  "^f": ["Sent", "in:sent"],
  "^r": ["Drafts", "in:drafts"],
  "^s": ["Spam", "in:spam"],
  "^k": ["Trash", "in:trash"],
  "^t": ["Starred", "is:starred"],
  "^b": ["Chats", "in:chats"],
  "^all": ["All Mail", "in:anywhere"],
  "^io_im": ["Important", "is:important"],
  "^scheduled": ["Scheduled", "in:scheduled"],
  "^smartlabel_personal": ["Primary", "category:primary"],
  "^smartlabel_social": ["Social", "category:social"],
  "^smartlabel_promo": ["Promotions", "category:promotions"],
  "^smartlabel_notification": ["Updates", "category:updates"],
  "^smartlabel_group": ["Forums", "category:forums"],
  "^smartlabel_finance": ["Purchases", "category:purchases"],
  "^smartlabel_receipt": ["Receipts", "category:purchases"],
  "^smartlabel_travel": ["Travel", "category:travel"],
};
const LIST_HIDDEN = new Set(["^all", "^io_unim", "^u", "^smartlabel_finance", "^smartlabel_receipt", "^smartlabel_travel"]);

/** Label ids to the names people see; internal markers (^u, ^io_unim, ^us, ...) are dropped. */
function labelNames(ids: unknown, users: Map<string, string>): string[] {
  const out: string[] = [];
  for (const id of arr(ids).map(str)) {
    if (id.startsWith("^")) { if (SYSTEM_LABELS[id] && !LIST_HIDDEN.has(id)) out.push(SYSTEM_LABELS[id][0]); }
    else out.push(users.get(id) ?? id);
  }
  return out;
}

const fmtAddr = (a: unknown): string => {
  const [, email, name] = arr(a);
  return str(name) ? `${str(name)} <${str(email)}>` : str(email);
};

async function userLabels(): Promise<Map<string, string>> {
  const p = await gmailPage();
  const raw = await p.evaluate(() => (window as unknown as Win).__blazeGmailBoot.hqyl8 ?? null);
  const m = new Map<string, string>();
  if (typeof raw !== "string") return m;
  for (const e of arr(arr(arr(JSON.parse(raw))[2])[0])) {
    const [id, name] = arr(arr(e)[0]);
    if (str(id) && !str(id).startsWith("^")) m.set(str(id), str(name) || str(id));
  }
  return m;
}

function searchBody(query: string, limit: number, page: number): unknown {
  const now = Date.now();
  const tz = -new Date().getTimezoneOffset() * 60_000;
  // Positions as Gmail's client sends them for a search: [0][1] page size, [0][3] query, [0][9] page index.
  return [[123, limit, null, query, [null, null, null, null, 0, null, null, null, null, null, null, now, tz, null, null, null, null, null, null, null, null, null, null, 0, 0, 0, 0],
    `itemlist-ViewType(123)-${page * 2}`, 1, 2000, null, page, null, null, null, 1, null,
    [1, 0, 0, null, null, 0, 1, null, null, 1, null, null, 1, null, 1, null, null, null, null, null, 0],
    null, null, 1, null, null, 0, 1, 0, [[[2, limit]]], 0, 0, null, null, null, null, null, [now - 30 * 86_400_000, null, null, 1]],
    null, [0, 5, null, null, 1, 1, 1]];
}

/** The raw thread entries of a search. */
async function searchRaw(query: string, limit: number, page: number): Promise<A[]> {
  if (!Number.isInteger(limit) || limit < 1 || limit > 100) throw new GmailError(`gmail.search: limit must be 1..100, got ${limit}`);
  if (!Number.isInteger(page) || page < 0) throw new GmailError(`gmail.search: page must be 0, 1, 2, ..., got ${page}`);
  // bv honours the page size ([0][1]) but not the page index ([0][9]; the client pages with state we don't keep), so
  // page n is rows n*limit .. (n+1)*limit of one request for (n+1)*limit rows.
  const want = (page + 1) * limit;
  if (want > 500) throw new GmailError(`gmail.search: page ${page} of ${limit} needs ${want} rows; at most 500 (narrow the query instead)`);
  const r = await sync("bv", searchBody(query, want, 0));
  const count = arr(r[11])[1];
  if (count === 0) return [];
  const rows = arr(arr(arr(r[19])[0])[1]);
  if (!rows.length && typeof count !== "number")
    throw new GmailError(`Gmail endpoint ${SYNC}/bv: reply has no thread list at [19][0][1] and no count at [11][1]; the reply format changed`);
  return rows.map((x) => arr(arr(x)[0])).filter((t) => str(t[3]).startsWith("thread-")).slice(page * limit, want);
}

function threadRow(t: A, users: Map<string, string>): GmailThread {
  // t: [0] subject, [1] snippet, [2] date ms, [3] thread id, [4] messages: [0] id, [1] [_, email, name], [6] date, [9] snippet, [10] labels
  const msgs = arr(t[4]).map(arr);
  const last = msgs[msgs.length - 1] ?? [];
  const ids = new Set(msgs.flatMap((m) => arr(m[10]).map(str)));
  const [, email, name] = arr(last[1]);
  return {
    id: str(t[3]), subject: str(t[0]), from: str(name) || str(email), fromEmail: str(email),
    date: new Date(num(t[2])), snippet: str(t[1]), labels: labelNames([...ids], users), unread: ids.has("^u"), messages: msgs.length,
  };
}

const ENTITIES: Record<string, string> = { amp: "&", lt: "<", gt: ">", quot: '"', apos: "'", nbsp: " ", ndash: "–", mdash: "—", hellip: "…", rsquo: "’", lsquo: "‘", rdquo: "”", ldquo: "“", copy: "©", reg: "®", trade: "™", middot: "·", bull: "•", zwnj: "", zwj: "", shy: "" };
const decode = (s: string) => s.replace(/&(#x[0-9a-f]+|#\d+|[a-z]+);/gi, (m, e: string) => {
  if (e[0] === "#") { const n = e[1] === "x" || e[1] === "X" ? parseInt(e.slice(2), 16) : parseInt(e.slice(1), 10); return Number.isFinite(n) ? String.fromCodePoint(n) : m; }
  return ENTITIES[e.toLowerCase()] ?? m;
});

const BLOCK = new Set(["p", "div", "tr", "li", "h1", "h2", "h3", "h4", "h5", "h6", "table", "ul", "ol", "blockquote", "pre", "hr", "section", "article", "header", "footer", "center", "dl", "dt", "dd"]);
// Gmail (.gmail_quote), Apple Mail and Thunderbird (blockquote[type=cite]), Yahoo, Outlook web: quoted history.
const QUOTE = ".gmail_quote, blockquote[type=cite], .yahoo_quoted, #mail-editor-reference-message-container";
// Outlook: everything from its reply header on is history.
const CUT_FROM = "#appendonsend, #divRplyFwdMsg";

/** A message body (Gmail hands back HTML) as plain text, with quoted history cut where it is recognisable.
 *  Done here with Bun's HTMLRewriter, not in the page: Gmail's page enforces Trusted Types, so DOMParser refuses raw HTML. */
export function bodyText(html: string): { text: string; quotedRemoved: boolean } {
  let quotedRemoved = false;
  const isHtml = /<(?:html|body|div|p|br|span|table|td|a|font|img|blockquote|ul|ol|li|h[1-6]|hr|center|pre)(?:\s[^>]*)?\/?>/i.test(html);
  let text = html;
  if (isHtml) {
    const out: string[] = [];
    let skip = 0, pre = 0, cut = false;
    const keepForward = /-{5,}\s*Forwarded message/i.test(html); // a forward's body is the content, not history
    const enter = (el: HTMLRewriterTypes.Element, onEnd: () => void) => { if (!el.selfClosing && !VOID.has(el.tagName)) el.onEndTag(onEnd); };
    const VOID = new Set(["br", "hr", "img", "meta", "link", "input", "col", "area", "base", "wbr", "source"]);
    const links: (string | null)[] = [];
    new HTMLRewriter()
      .on("style, script, head, title", { element(el) { skip++; enter(el, () => { skip--; }); } })
      .on(QUOTE, { element(el) { if (keepForward || skip || cut) return; skip++; quotedRemoved = true; enter(el, () => { skip--; }); } })
      .on(CUT_FROM, { element() { if (!skip && !cut) { cut = true; quotedRemoved = true; } } })
      .on("*", {
        element(el) {
          const tag = el.tagName;
          if (skip || cut) return;
          if (tag === "br") { out.push("\n"); return; }
          if (tag === "pre") { pre++; enter(el, () => { pre--; }); }
          if (tag === "td" || tag === "th") out.push(" ");
          if (BLOCK.has(tag)) { out.push("\n"); enter(el, () => { if (!skip && !cut) out.push("\n"); }); }
          if (tag === "a") {
            const href = el.getAttribute("href") ?? "";
            const mark = out.length;
            links.push(href);
            enter(el, () => {
              links.pop();
              if (skip || cut) return;
              const label = decode(out.slice(mark).join("")).trim();
              if (/^https?:/i.test(href) && href.length <= 300 && label && label !== href) out.push(` <${decode(href)}>`);
            });
          }
        },
      })
      .onDocument({ text(t) { if (!skip && !cut) out.push(pre ? t.text : t.text.replace(/\s+/g, " ")); } })
      .transform(html);
    text = decode(out.join(""));
  }
  let lines = text.split("\n").map((l) => (isHtml ? l.replace(/^[ \t ]+|[ \t ]+$/g, "") : l.replace(/[ \t ]+$/g, "")));
  // Plain-text history: "On ... wrote:" followed by "> " lines, "-----Original Message-----", Outlook "From:/Sent:".
  const at = lines.findIndex((l, i) =>
    (/^On .+wrote:\s*$/.test(l) && /^\s*(>|$)/.test(lines[i + 1] ?? "") && lines.slice(i + 1).some((x) => x.startsWith(">"))) ||
    /^-{3,}\s*Original Message\s*-{3,}$/i.test(l.trim()) ||
    (/^From: .+/.test(l) && /^(Sent|Date): /.test(lines[i + 1] ?? "")));
  if (at > 0) { lines = lines.slice(0, at); quotedRemoved = true; }
  while (lines.length && /^\s*>/.test(lines[lines.length - 1])) { lines.pop(); quotedRemoved = true; }
  return { text: lines.join("\n").replace(/\n{3,}/g, "\n\n").trim(), quotedRemoved };
}

async function fetchThreads(ids: string[]): Promise<A[]> {
  const r = await sync("fd", [ids.map((id) => [id, 1, null, null, 1]), 2]);
  const threads = arr(r[1]).map(arr);
  for (const id of ids)
    if (!threads.some((t) => t[0] === id && arr(t[2]).length))
      throw new GmailError(`Gmail endpoint ${SYNC}/fd returned no messages for ${id}: no such thread in this account, or the reply format changed`);
  return threads;
}

async function decodeThread(t: A, users: Map<string, string>): Promise<GmailThreadDetail> {
  // t: [0] id, [1] [summary, [msg summaries: [0] id, [3] labels]], [2] messages: [0] id, [1] details
  const [summary, sums] = arr(t[1]);
  // Summaries: [0] id, [1] [_, email, name] sender, [3] labels.
  const sumById = new Map(arr(sums).map(arr).map((s) => [str(s[0]), s]));
  const labelsById = new Map([...sumById].map(([id, s]) => [id, arr(s[3]).map(str)]));
  const msgs = arr(t[2]).map(arr);
  const details = msgs.map((m) => arr(m[1]));
  // details: [0] to, [1] cc, [2] bcc, [3] from, [4] subject, [5][1] body parts ([2][1] content), [7] Message-ID,
  // [13] attachments ([0]: [1] part id, [3] [_, url, name, mime, size]), [16] date ms
  const bodies = details.map((d) => arr(arr(d[5])[1]).map((part) => str(arr(arr(part)[2])[1])).join("\n"));
  const texts = bodies.map(bodyText);
  const messages = msgs.map((m, i): GmailMessage => {
    const d = details[i];
    const id = str(m[0]);
    const ids = labelsById.get(id) ?? [];
    // The sender is in the details ([3][0]) for most mail, only in the summary for some (Google's own alerts).
    const [, email, name] = arr(d[3]).length ? arr(arr(d[3])[0]) : arr(arr(sumById.get(id))[1]);
    return {
      id, threadId: str(t[0]), subject: str(d[4]),
      from: str(name) || str(email), fromEmail: str(email),
      to: arr(d[0]).map(fmtAddr), cc: arr(d[1]).map(fmtAddr), bcc: arr(d[2]).map(fmtAddr),
      date: new Date(num(d[16])), labels: labelNames(ids, users), unread: ids.includes("^u"), messageId: str(d[7]),
      text: texts[i].text, quotedRemoved: texts[i].quotedRemoved,
      attachments: table(arr(d[13]).map((a) => {
        const p = arr(arr(a)[0]);
        const meta = arr(p[3]);
        return { partId: str(p[1]), name: str(meta[2]), mime: str(meta[3]), size: num(meta[4]) };
      })),
    };
  });
  const all = [...labelsById.values()].flat();
  return { id: str(t[0]), subject: str(arr(summary)[1]) || messages[0]?.subject || "", labels: labelNames([...new Set(all)], users), unread: all.includes("^u"), messages: table(messages) };
}

const plural = (n: number, w: string) => `${n} ${w}${n === 1 ? "" : "s"}`;

// ---------------------------------------------------------------- the calls

/** Threads matching a Gmail search (the same syntax as Gmail's search box), newest first. */
function search(query: string, opts: { limit?: number; page?: number } = {}): Promise<Table<GmailThread>> {
  const { limit = 25, page = 0 } = opts;
  const q = JSON.stringify(oneLine(query, 60));
  return effect("search", `Searching Gmail for ${q}`, `Searched Gmail for ${q}`, query, async () => {
    const [rows, users] = await Promise.all([searchRaw(query, limit, page), userLabels()]);
    return table(rows.map((t) => threadRow(t, users)));
  }, (r) => ({ detail: plural(r.length, "thread"), show: r.select("subject", "from", "date", "unread", "labels") }));
}

/** Messages (not threads) matching a search: each listed message of each matching thread. No bodies; `read` for those. */
function messages(query: string, opts: { limit?: number; page?: number } = {}): Promise<Table<GmailMessageRow>> {
  const { limit = 25, page = 0 } = opts;
  const q = JSON.stringify(oneLine(query, 60));
  return effect("search", `Searching Gmail messages for ${q}`, `Searched Gmail messages for ${q}`, query, async () => {
    const [rows, users] = await Promise.all([searchRaw(query, limit, page), userLabels()]);
    return table(rows.flatMap((t) => arr(t[4]).map(arr).map((m): GmailMessageRow => {
      const [, email, name] = arr(m[1]);
      const ids = arr(m[10]).map(str);
      return {
        id: str(m[0]), threadId: str(t[3]), subject: str(t[0]), from: str(name) || str(email), fromEmail: str(email),
        date: new Date(num(m[6])), snippet: str(m[9]), labels: labelNames(ids, users), unread: ids.includes("^u"),
      };
    })));
  }, (r) => ({ detail: plural(r.length, "message"), show: r.select("subject", "from", "date", "unread") }));
}

type ReadTarget = string | { id: string; threadId?: string };

/** A whole thread with plain-text bodies: a thread id ("thread-f:..."), a row from search/messages, or a message
 *  row (the thread is read, with `message` set to that one). Several at once: pass an array (one request). */
function read(target: ReadTarget): Promise<GmailThreadDetail & { message?: GmailMessage }>;
function read(target: ReadTarget[]): Promise<Table<GmailThreadDetail>>;
function read(target: ReadTarget | ReadTarget[]): Promise<unknown> {
  const many = Array.isArray(target);
  const targets = (many ? target : [target]).map((t) => {
    const id = typeof t === "string" ? t : t.threadId ?? t.id;
    if (!id.startsWith("thread-"))
      throw new GmailError(`gmail.read(${JSON.stringify(id)}): needs a thread id ("thread-f:..."); for a message pass its row from gmail.messages(), which carries threadId`);
    return { thread: id, msg: typeof t !== "string" && t.threadId ? t.id : undefined };
  });
  const what = many ? plural(targets.length, "Gmail thread") : "a Gmail thread";
  return effect("read", `Reading ${what}`, `Read ${what}`, targets.map((t) => t.thread).join(" "), async () => {
    const [raw, users] = await Promise.all([fetchThreads([...new Set(targets.map((t) => t.thread))]), userLabels()]);
    const byId = new Map<string, GmailThreadDetail>();
    for (const t of raw) byId.set(str(t[0]), await decodeThread(t, users));
    const out = targets.map(({ thread, msg }) => {
      const d = byId.get(thread)!;
      if (!msg) return d;
      const message = d.messages.find((m) => m.id === msg);
      if (!message) throw new GmailError(`gmail.read: ${msg} is not in ${thread}`);
      return { ...d, message };
    });
    return many ? table(out) : out[0];
  }, (r) => {
    const d = r as GmailThreadDetail | Table<GmailThreadDetail>;
    const first = Array.isArray(d) ? d[0] : d;
    return { detail: Array.isArray(d) ? plural(d.length, "thread") : `${oneLine(first.subject, 50)} · ${plural(first.messages.length, "message")}` };
  });
}

/** Every label: system ones (Inbox, Sent, Starred, categories...) and the account's own. */
function labels(): Promise<Table<GmailLabel>> {
  return effect("read", "Reading Gmail labels", "Read Gmail labels", "labels", async () => {
    const p = await gmailPage();
    const raw = await p.evaluate(() => (window as unknown as Win).__blazeGmailBoot.hqyl8 ?? null);
    if (typeof raw !== "string")
      throw new GmailError("Gmail's boot data has no label list (key hqyl8 of _GM_setData); the web client changed");
    const out: GmailLabel[] = [];
    for (const e of arr(arr(arr(JSON.parse(raw))[2])[0])) {
      const [id, name] = arr(arr(e)[0]).map(str);
      if (!id) continue;
      if (id.startsWith("^")) {
        const known = SYSTEM_LABELS[id];
        if (known && !LIST_HIDDEN.has(id)) out.push({ id, name: known[0], system: true, query: known[1] });
      } else out.push({ id, name: name || id, system: false, query: `label:${JSON.stringify(name || id).replace(/^"(\S+)"$/, "$1")}` });
    }
    if (!out.some((l) => l.id === "^i")) throw new GmailError("Gmail's label list (boot data hqyl8) has no Inbox (^i); its format changed");
    return table(out);
  }, (r) => ({ detail: plural(r.length, "label") }));
}

/** Save one attachment to a file and return its path (default under $HARNESS_HOME/gmail/attachments/<msg>/). */
function attachment(threadId: string, msgId: string, partId: string, opts: { dir?: string } = {}): Promise<string> {
  return effect("write", `Saving Gmail attachment ${partId}`, `Saved Gmail attachment ${partId}`, `${msgId} ${partId}`, async () => {
    const [t] = await fetchThreads([threadId]);
    const m = arr(t[2]).map(arr).find((m) => m[0] === msgId);
    if (!m) throw new GmailError(`gmail.attachment: ${msgId} is not in ${threadId}`);
    const part = arr(arr(m[1])[13]).map((a) => arr(arr(a)[0])).find((p) => p[1] === partId);
    if (!part) throw new GmailError(`gmail.attachment: ${msgId} has no attachment part ${partId} (its parts: ${arr(arr(m[1])[13]).map((a) => str(arr(arr(a)[0])[1])).join(", ") || "none"})`);
    const [, url, name] = arr(part[3]).map(str);
    if (!url) throw new GmailError(`Gmail endpoint ${SYNC}/fd: attachment ${partId} of ${msgId} has no download URL at [3][1]; the reply format changed`);
    const p = await gmailPage();
    const r = await p.evaluate(async (url) => {
      const res = await fetch(url, { credentials: "include" });
      const type = res.headers.get("content-type") ?? "";
      if (res.status !== 200) return { status: res.status, type, b64: "" };
      const bytes = new Uint8Array(await res.arrayBuffer());
      let s = "";
      for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
      return { status: res.status, type, b64: btoa(s) };
    }, url);
    const path = new URL(url, ORIGIN).pathname;
    if (r.status !== 200) throw new GmailError(`Gmail attachment download GET ${path}?view=att returned HTTP ${r.status}`);
    if (/text\/html/i.test(r.type)) throw new GmailError(`Gmail attachment download GET ${path}?view=att returned an HTML page, not the file (signed out, or the URL scheme changed)`);
    const safe = (name || `attachment-${partId}`).replace(/[/\\\0]/g, "_").replace(/^\.+/, "_");
    const dir = opts.dir ?? join(HOME(), "gmail", "attachments", msgId.replace(/[^\w.-]/g, "_"));
    await mkdir(dir, { recursive: true });
    const file = join(dir, safe);
    await Bun.write(file, Buffer.from(r.b64, "base64"));
    return file;
  }, (f) => ({ detail: f }));
}

export const gmail = { search, messages, read, labels, attachment };
