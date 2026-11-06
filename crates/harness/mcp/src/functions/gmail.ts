// Gmail through Gmail's own web client: a mail.google.com page in the harness browser (embedded Chromium, cdp.ts),
// whose requests carry the session the person signed in to once in the browser pane (cmd+shift+B). No public Gmail API,
// no OAuth. `gmail.via({ cdp })` (or HARNESS_GMAIL_CDP) points these helpers, and only them, at another browser's
// DevTools endpoint instead: an explicit choice, never made on its own and never tried when the harness browser fails.
//
// How it works. The web client talks JSON arrays ("pt=ji") to /sync/u/<n>/i/<op>:
//   bv  "bulk view": a search; the request is the query plus paging, the reply the matching threads with a
//       summary of their messages.
//   fd  "fetch details": whole threads, every message with headers, its HTML body and attachment parts.
//   s   "sync": every change. A request is a list of thread operations [opNum, [threadId, fields]]; the fields used:
//       3  create a thread: [[subject, null, ts, threadId, [message]]] (a new draft)
//       7  change labels: [add, remove, messageIds] (archive = remove ^i, trash = add ^k, read = remove ^u)
//       10 discard draft messages: [messageIds]
//       14 save a message: [message, null, 1]; with the draft labels (^r, ^r_bt) it is a draft, with the outgoing set
//          (^pfg, ^f_bt, ^f_btns, ^f_cl, ^a) Gmail sends it. Ids are the client's own: thread-a:r<int64>, msg-a:r<int64>.
//       The reply lists [opNum, status] per operation; status 0 is applied.
// Those requests need headers the client computes (an XSRF token, a client-info blob, a storage flag). We don't
// rebuild them: a script installed before the page loads records the X-* headers Gmail's own XHRs send to /sync/,
// and our requests reuse them from inside the page. The values stay in the page; nothing here reads, logs or returns
// them. Label names come from the boot data the page hands to `_GM_setData` (key hqyl8), recorded the same way.
// Attachments download from the legacy `/mail/u/<n>/?view=att` URL the fd reply names, and upload to /_/upload (a
// resumable upload whose reply is a blob token the message names).
//
// The array positions below were read off live traffic (2026-11-05); when Gmail moves them, the error names the
// endpoint and what was missing.
import { mkdir } from "node:fs/promises";
import { join } from "node:path";
import { external, newPage, type Page } from "../cdp.ts";
import { table, type Table } from "../table.ts";
import { effect, oneLine } from "../effects.ts";
import { canConfirm, confirm } from "../host.ts";

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
  if (p.closed) return false;
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

/** Where the gmail helpers' page lives: the harness browser (null), or the DevTools endpoint gmail.via chose. */
let via: string | null | undefined;
const endpoint = (): string | null => via !== undefined ? via : process.env.HARNESS_GMAIL_CDP || null;

/**
 * Point the gmail helpers at the DevTools endpoint `cdp` (`"http://127.0.0.1:9222"`: a browser started with
 * `--remote-debugging-port`, where the person is signed in to Gmail) for the rest of this kernel's life; `cdp: null` sends
 * them back to the harness browser. Only gmail.* use it; nothing else does, and nothing switches to it on a failure.
 */
function via_(opts: { cdp: string | null }): void {
  if (opts.cdp !== null && !/^https?:\/\/[^/\s]+\/?$/.test(opts.cdp))
    throw new GmailError(`gmail.via: cdp must be a DevTools endpoint like "http://127.0.0.1:9222" (or null for the harness browser), got ${JSON.stringify(opts.cdp)}`);
  via = opts.cdp === null ? null : opts.cdp.replace(/\/$/, "");
  const old = shared;
  shared = undefined;
  old?.then((p) => p.close(), () => {});
}

async function openPage(): Promise<Page> {
  const at_ = endpoint();
  const p = at_ === null ? await newPage() : await (await external(at_)).newPage();
  try {
    await p.send("Page.addScriptToEvaluateOnNewDocument", { source: HOOK });
    await p.goto(`${ORIGIN}/mail/u/${ACCOUNT}/#inbox`, 45_000);
    const at = new URL(await p.url());
    if (at.host !== "mail.google.com" || !at.pathname.startsWith("/mail/"))
      throw new GmailError(at_ === null
        ? `Gmail is not signed in in the harness browser: mail.google.com/mail/u/${ACCOUNT}/ redirected to ${at.host}${at.pathname}. Open mail.google.com in the browser pane (cmd+shift+B), sign in to Google, and retry. If Google refuses to sign in there, gmail.via({ cdp: "http://127.0.0.1:9222" }) uses a browser started with --remote-debugging-port instead.`
        : `Gmail is not signed in in the browser at ${at_} (gmail.via / HARNESS_GMAIL_CDP): mail.google.com/mail/u/${ACCOUNT}/ redirected to ${at.host}${at.pathname}. Sign in to Gmail there and retry, or gmail.via({ cdp: null }) for the harness browser.`);
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
async function sync(op: "bv" | "fd" | "s", body: unknown, retried = false): Promise<unknown[]> {
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
              if (/^https?:/i.test(href) && href.length <= 120 && label && label !== href) out.push(` <${decode(href)}>`);
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

// ---------------------------------------------------------------- writing

/** One address ("a@b" or "Name <a@b>") or several (an array, or comma separated). */
type Addrs = string | string[];

/** A message to send or keep as a draft. */
export type GmailOutgoing = {
  to: Addrs;
  cc?: Addrs;
  bcc?: Addrs;
  subject: string;
  /** Plain text; it becomes the HTML body (escaped, line breaks kept) unless `html` is given. */
  body?: string;
  html?: string;
  /** Paths of files to attach (25 MB in all at most, Gmail's limit). */
  attachments?: string[];
  /** Reply in this message's thread: a message (a row from gmail.messages(), or read(...).messages[i]). */
  replyTo?: MessageRef;
};

/** A draft gmail.draft saved: send it with gmail.send(draft), discard it with gmail.deleteDraft(draft). */
export type GmailDraft = { id: string; threadId: string; subject: string; to: string[]; cc: string[]; bcc: string[]; attachments: string[] };

/** A message: its row ({ id, threadId }), or a bare "msg-f:…" id (found when it starts its thread). */
type MessageRef = string | { id: string; threadId?: string };
/** What label changes act on: a thread id or row (every message in it), or a message row (that one). */
type Target = string | { id: string; threadId?: string };

// The labels Gmail's client gives a draft, and the set that makes a save a send (from its own label builder:
// ^pfg "pending send", ^f_bt/^f_btns/^f_cl sent bookkeeping, ^a; the draft labels removed).
const DRAFT_LABELS = ["^all", "^r", "^r_bt"];
const SEND_LABELS = ["^all", "^pfg", "^f_bt", "^f_btns", "^f_cl", "^a"];
const MAX_ATTACH = 25 * 1024 * 1024;

const rand64 = () => BigInt.asIntN(64, crypto.getRandomValues(new BigUint64Array(1))[0]!).toString();
const hex = (n: number) => [...crypto.getRandomValues(new Uint8Array(n))].map((b) => b.toString(16).padStart(2, "0")).join("");

const addrList = (v: Addrs | undefined): string[] =>
  (v === undefined ? [] : Array.isArray(v) ? v : [v]).flatMap((x) => x.split(/,(?=(?:[^"]*"[^"]*")*[^"]*$)/)).map((x) => x.trim()).filter(Boolean);

/** "Name <a@b>" or "a@b" as Gmail's address entry [1, email, name?]. */
function addrEntry(s: string): A {
  const m = /^\s*"?([^"<]*?)"?\s*<([^>]+)>\s*$/.exec(s);
  const email = (m ? m[2] : s).trim();
  const name = m?.[1]?.trim();
  if (!/^[^\s@<>,"]+@[^\s@<>,"]+\.[^\s@<>,"]+$/.test(email)) throw new GmailError(`gmail: ${JSON.stringify(s)} is not an email address`);
  return name ? [1, email, name] : [1, email];
}

const escapeHtml = (t: string) => t.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
const textHtml = (t: string) => `<div dir="ltr">${escapeHtml(t).replace(/\r?\n/g, "<br>")}</div>`;

type Me = { email: string; name: string };
let meCache: Me | undefined;
/** The signed-in account: its address (GLOBALS[10]) and display name (the account button's label). */
async function me(): Promise<Me> {
  if (meCache) return meCache;
  const p = await gmailPage();
  const r = await p.evaluate(() => {
    const email = (window as unknown as { GLOBALS?: unknown[] }).GLOBALS?.[10];
    return { email: typeof email === "string" ? email : "", label: document.querySelector("a[aria-label^='Google Account:']")?.getAttribute("aria-label") ?? "" };
  });
  if (!r.email) throw new GmailError("Gmail's page has no account address at GLOBALS[10]; the web client changed");
  return (meCache = { email: r.email, name: /^Google Account:\s*(.*?)\s*\(/.exec(r.label)?.[1] ?? "" });
}

/** A message as the web client's own draft saves lay it out (78 slots; the positions used are the ones it fills). */
export function messageArray(o: { id: string; from: Me; to: A[]; cc: A[]; bcc: A[]; subject: string; html: string; labels: string[]; attachments: A[]; session: string; ts: number }): A {
  const m: A = Array(78).fill(null);
  m[0] = o.id;
  m[1] = [1, o.from.email, o.from.name || null, null, null, null, null, null, null, o.from.email];
  m[2] = o.to;
  m[3] = o.cc.length ? o.cc : null;
  m[4] = o.bcc.length ? o.bcc : null;
  m[6] = o.ts;
  m[7] = o.subject;
  m[8] = [null, [[0, o.html]], null, null, null, null, 1];
  m[10] = o.labels;
  m[11] = o.attachments.length ? o.attachments : null;
  m[17] = o.ts;
  m[41] = 0;
  m[42] = [0, 0, 0, null, 0];
  const s = `s:${o.session}|#${o.id}|0`; // the compose session the message belongs to
  m[51] = s;
  m[76] = [null, null, null, null, null, null, s];
  m[77] = [];
  return m;
}

type Op = { thread: string; field: number; payload: unknown };
/** A /i/s request: each operation is field `field` (1-based) of a thread's change. */
export function syncRequest(ops: Op[], now = Date.now()): unknown {
  return [null, [ops.map((o, i) => {
    const f: A = Array(o.field).fill(null);
    f[o.field - 1] = o.payload;
    return [i + 1, [o.thread, f]];
  })], [1, null, null, null, [null, 0], null, 1], [null, 1, now, 0, 1], 2];
}

/** Apply thread operations; every one must come back with status 0. */
async function mutate(what: string, ops: Op[]): Promise<void> {
  const r = await sync("s", syncRequest(ops));
  const results = arr(arr(r[0])[0]).map(arr);
  ops.forEach((o, i) => {
    const res = results.find((x) => x[0] === i + 1);
    if (!res) throw new GmailError(`Gmail endpoint POST ${SYNC}/s: no result for ${what} on ${o.thread} (operation ${i + 1}); the reply format changed`);
    if (res[1] !== 0) throw new GmailError(`Gmail endpoint POST ${SYNC}/s refused ${what} on ${o.thread}: status ${JSON.stringify(res[1])}`);
  });
}

/** Protobuf for an upload's start request: 1 message id, 3 0, 4 0, 7 and 8 the account, 11 0. */
function uploadStartBody(msgId: string, email: string): string {
  const out: number[] = [];
  const varint = (n: number) => { while (n > 0x7f) { out.push((n & 0x7f) | 0x80); n >>>= 7; } out.push(n); };
  const string = (field: number, s: string) => { const b = new TextEncoder().encode(s); varint((field << 3) | 2); varint(b.length); out.push(...b); };
  const zero = (field: number) => { varint(field << 3); varint(0); };
  string(1, msgId); zero(3); zero(4); string(7, email); string(8, email); zero(11);
  return Buffer.from(out).toString("base64");
}

type Upload = { path: string; name: string; mime: string; bytes: Uint8Array };
async function readAttachments(paths: string[]): Promise<Upload[]> {
  const out: Upload[] = [];
  for (const path of paths) {
    const f = Bun.file(path);
    if (!(await f.exists())) throw new GmailError(`gmail: attachment ${path} does not exist`);
    out.push({ path, name: path.split("/").pop() || path, mime: f.type.split(";")[0] || "application/octet-stream", bytes: new Uint8Array(await f.arrayBuffer()) });
  }
  const total = out.reduce((n, u) => n + u.bytes.length, 0);
  if (total > MAX_ATTACH) throw new GmailError(`gmail: attachments total ${(total / 1048576).toFixed(1)} MB; Gmail takes at most 25 MB (${out.map((u) => u.name).join(", ")})`);
  return out;
}

/** Upload one file for message `msgId` (Gmail's resumable /_/upload) and return its attachment entry. */
async function upload(u: Upload, msgId: string, my: Me): Promise<A> {
  const p = await gmailPage();
  const r = await p.evaluate(async ({ start, name, mime, b64 }) => {
    const bin = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
    const s = await fetch("/_/upload?authuser=0&dcp=asu-n", {
      method: "POST", credentials: "include", body: start,
      headers: { "X-Goog-Upload-Protocol": "resumable", "X-Goog-Upload-Command": "start", "X-Goog-Upload-File-Name": name,
        "X-Goog-Upload-Header-Content-Type": mime, "X-Goog-Upload-Header-Content-Length": String(bin.length), "Content-Type": "application/x-www-form-urlencoded;charset=UTF-8" },
    });
    const url = s.headers.get("x-goog-upload-url");
    if (s.status !== 200 || !url) return { step: "start", status: s.status, token: "" };
    const f = await fetch(url, { method: "POST", credentials: "include", body: bin, headers: { "X-Goog-Upload-Command": "upload, finalize", "X-Goog-Upload-Offset": "0", "X-Goog-Upload-File-Name": name, "Content-Type": mime } });
    return { step: "upload", status: f.status, token: f.status === 200 ? (await f.text()).trim() : "" };
  }, { start: uploadStartBody(msgId, my.email), name: /^[\x20-\x7e]*$/.test(u.name) ? u.name : encodeURIComponent(u.name), mime: u.mime, b64: Buffer.from(u.bytes).toString("base64") });
  if (!r.token) throw new GmailError(`Gmail endpoint POST /_/upload (${r.step}) returned HTTP ${r.status} for attachment ${u.path}${r.status === 200 ? " without an upload URL or token; the upload scheme changed" : ""}`);
  return [u.mime, u.name, u.bytes.length, null, `f_${Math.random().toString(36).slice(2, 10)}`, null, 0, [], null, r.token];
}

/** A message with what a reply or forward needs: its thread, its decoded fields and its HTML body. */
async function original(ref: MessageRef, what: string): Promise<{ thread: GmailThreadDetail; message: GmailMessage; html: string }> {
  const id = typeof ref === "string" ? ref : ref.id;
  const threadId = typeof ref === "string" ? undefined : ref.threadId;
  if (!id.startsWith("msg-")) throw new GmailError(`gmail.${what}: ${JSON.stringify(id)} is not a message id ("msg-f:…"); pass a row from gmail.messages() or read(...).messages`);
  // A thread is named after its first message, so a bare id finds its thread only when it is the first one.
  const tid = threadId ?? id.replace(/^msg-/, "thread-");
  const raw = await sync("fd", [[[tid, 1, null, null, 1]], 2]);
  const t = arr(raw[1]).map(arr).find((x) => x[0] === tid);
  const m = arr(t?.[2]).map(arr).find((x) => x[0] === id);
  if (!t || !m) throw new GmailError(`gmail.${what}: ${id} is not in thread ${tid}${threadId ? "" : "; pass the message's row (with threadId) from gmail.messages() or read(...).messages"}`);
  const thread = await decodeThread(t, await userLabels());
  const html = arr(arr(arr(m[1])[5])[1]).map((part) => str(arr(arr(part)[2])[1])).join("\n");
  return { thread, message: thread.messages.find((x) => x.id === id)!, html };
}

const emailOf = (s: string) => (/<([^>]+)>\s*$/.exec(s)?.[1] ?? s).trim().toLowerCase();
const when = (d: Date) => d.toLocaleString("en-US", { weekday: "short", month: "short", day: "numeric", year: "numeric", hour: "numeric", minute: "2-digit" });
const quoteBlock = (m: GmailMessage, html: string) =>
  `<br><div class="gmail_quote"><div dir="ltr" class="gmail_attr">On ${escapeHtml(when(m.date))}, ${escapeHtml(m.from)} &lt;${escapeHtml(m.fromEmail)}&gt; wrote:<br></div>` +
  `<blockquote class="gmail_quote" style="margin:0px 0px 0px 0.8ex;border-left:1px solid rgb(204,204,204);padding-left:1ex">${html || textHtml(m.text)}</blockquote></div>`;

/** Everything a save or send needs, checked before anything is written. */
type Prepared = {
  thread: string; id: string; newThread: boolean; from: Me;
  to: string[]; cc: string[]; bcc: string[]; subject: string; html: string; preview: string;
  uploads: Upload[]; inReplyTo?: GmailMessage; attached: A[];
};

async function prepare(o: GmailOutgoing, what: string): Promise<Prepared> {
  const to = addrList(o.to), cc = addrList(o.cc), bcc = addrList(o.bcc);
  if (!to.length && !cc.length && !bcc.length) throw new GmailError(`gmail.${what}: no recipients (to, cc and bcc are all empty)`);
  [...to, ...cc, ...bcc].forEach(addrEntry);
  if (typeof o.subject !== "string") throw new GmailError(`gmail.${what}: subject must be a string`);
  if (o.body === undefined && o.html === undefined) throw new GmailError(`gmail.${what}: give a body (plain text) or html`);
  const [from, uploads] = await Promise.all([me(), readAttachments(o.attachments ?? [])]);
  let html = o.html ?? textHtml(o.body ?? "");
  let thread = `thread-a:r${rand64()}`, newThread = true, inReplyTo: GmailMessage | undefined;
  if (o.replyTo !== undefined) {
    const orig = await original(o.replyTo, what);
    inReplyTo = orig.message;
    thread = orig.thread.id;
    newThread = false;
    html += quoteBlock(orig.message, orig.html);
  }
  const preview = o.body ?? bodyText(o.html ?? "").text;
  return { thread, id: `msg-a:r${rand64()}`, newThread, from, to, cc, bcc, subject: o.subject, html, preview, uploads, inReplyTo, attached: [] };
}

function message(p: Prepared, labels: string[]): A {
  return messageArray({ id: p.id, from: p.from, to: p.to.map(addrEntry), cc: p.cc.map(addrEntry), bcc: p.bcc.map(addrEntry), subject: p.subject, html: p.html, labels, attachments: p.attached, session: hex(8), ts: Date.now() });
}

/** Upload the attachments and save `p` as a draft (a new thread, or a new message in the replied-to thread). */
async function saveDraft(p: Prepared): Promise<void> {
  if (p.uploads.length && !p.attached.length) p.attached = await Promise.all(p.uploads.map((u) => upload(u, p.id, p.from)));
  const m = message(p, DRAFT_LABELS);
  await mutate("saving a draft", [p.newThread
    ? { thread: p.thread, field: 3, payload: [[p.subject, null, m[6], p.thread, [m]]] }
    : { thread: p.thread, field: 14, payload: [m, null, 1] }]);
  p.newThread = false;
}

// Drafts this process saved, so send(draft) can send them as they are and deleteDraft needs no confirmation.
const ours = new Map<string, Prepared>();

const draftOf = (p: Prepared): GmailDraft => ({ id: p.id, threadId: p.thread, subject: p.subject, to: p.to, cc: p.cc, bcc: p.bcc, attachments: p.uploads.map((u) => u.name) });

/** Ask the person; refuse unless they accept. Nothing has been written when this throws. */
async function approve(what: string, text: string, nothing: string): Promise<void> {
  if (!canConfirm())
    throw new GmailError(`gmail.${what} refused: it needs the person's OK and this session has no way to ask (the MCP client offers no confirmation prompt); ${nothing}`);
  if (!(await confirm(text))) throw new GmailError(`gmail.${what}: the person declined; ${nothing}`);
}

function sendPrompt(p: Prepared): string {
  const lines = [`Send this email from ${p.from.email}?`, `To: ${p.to.join(", ") || "(none)"}`];
  if (p.cc.length) lines.push(`Cc: ${p.cc.join(", ")}`);
  if (p.bcc.length) lines.push(`Bcc: ${p.bcc.join(", ")}`);
  lines.push(`Subject: ${p.subject || "(no subject)"}`);
  if (p.inReplyTo) lines.push(`In reply to: ${p.inReplyTo.from} · ${oneLine(p.inReplyTo.subject, 60)}`);
  if (p.uploads.length) lines.push(`Attachments: ${p.uploads.map((u) => `${u.name} (${Math.max(1, Math.round(u.bytes.length / 1024))} KB)`).join(", ")}`);
  const body = p.preview.trim();
  lines.push("", body.length > 600 ? `${body.slice(0, 600)}…` : body || "(empty body)");
  return lines.join("\n");
}

/** Send: confirm with the person, then save and send. */
async function sendPrepared(p: Prepared, what: string): Promise<GmailDraft & { sent: true }> {
  await approve(what, sendPrompt(p), "nothing was sent");
  if (p.newThread) await saveDraft(p);
  else if (p.uploads.length && !p.attached.length) p.attached = await Promise.all(p.uploads.map((u) => upload(u, p.id, p.from)));
  try {
    await mutate("sending", [{ thread: p.thread, field: 14, payload: [message(p, SEND_LABELS), null, 1] }]);
  } catch (e) {
    throw new GmailError(`${(e as Error).message}; the message stays as draft ${p.id} in ${p.thread}`);
  }
  ours.delete(p.id);
  return { ...draftOf(p), sent: true };
}

const sendTarget = (p: Prepared) => `${p.to.concat(p.cc, p.bcc).join(", ")} · ${p.subject}`;

/** Send an email, or a draft gmail.draft saved. Always asks the person first (recipients, subject, body preview)
 *  and refuses when they decline or can't be asked. With replyTo it goes into that message's thread. */
function send(o: GmailOutgoing | GmailDraft): Promise<GmailDraft & { sent: true }> {
  return effect("net", "Sending an email", "Sent an email", "id" in o && "threadId" in o ? o.id : addrList((o as GmailOutgoing).to).join(", "), async () => {
    if ("id" in o && "threadId" in o) {
      const p = ours.get(o.id);
      if (!p) throw new GmailError(`gmail.send: draft ${o.id} was not saved by gmail.draft in this session, so its full content isn't known here; open it in Gmail to send it, or send a new one with gmail.send({ to, subject, body })`);
      return sendPrepared(p, "send");
    }
    return sendPrepared(await prepare(o, "send"), "send");
  }, (r) => ({ detail: `${r.to.concat(r.cc, r.bcc).join(", ")} · ${oneLine(r.subject, 50)}` }));
}

/** Save a draft (no confirmation; nothing leaves the account). Review it in Gmail, then gmail.send(draft) or deleteDraft. */
function draft(o: GmailOutgoing): Promise<GmailDraft> {
  return effect("write", "Saving a Gmail draft", "Saved a Gmail draft", addrList(o.to).join(", "), async () => {
    const p = await prepare(o, "draft");
    await saveDraft(p);
    ours.set(p.id, p);
    return draftOf(p);
  }, (d) => ({ detail: `${d.subject} → ${d.to.join(", ")}` }));
}

/** Discard a draft: one gmail.draft saved (no confirmation), or any other (asks the person). */
function deleteDraft(d: GmailDraft | { id: string; threadId: string }): Promise<void> {
  return effect("delete", "Discarding a Gmail draft", "Discarded a Gmail draft", d.id, async () => {
    if (!ours.has(d.id)) {
      const [t] = await fetchThreads([d.threadId]);
      const s = arr(arr(t[1])[1]).map(arr).find((x) => x[0] === d.id);
      if (!s) throw new GmailError(`gmail.deleteDraft: ${d.id} is not in ${d.threadId}`);
      if (!arr(s[3]).includes("^r")) throw new GmailError(`gmail.deleteDraft: ${d.id} is not a draft; it refuses to discard a sent or received message`);
      await approve("deleteDraft", `Discard the Gmail draft "${str(arr(arr(t[1])[0])[1]) || d.id}"?`, "the draft was kept");
    }
    await mutate("discarding a draft", [{ thread: d.threadId, field: 10, payload: [d.id] }]);
    ours.delete(d.id);
  });
}

/** Reply to a message (in its thread, quoting it). To the sender, or with { all: true } also everyone it went to.
 *  Asks the person first, like send. */
function reply(to: MessageRef, body: string, opts: { all?: boolean; html?: string; attachments?: string[]; cc?: Addrs; bcc?: Addrs } = {}): Promise<GmailDraft & { sent: true }> {
  return effect("net", "Replying to an email", "Replied to an email", typeof to === "string" ? to : to.id, async () => {
    const my = await me();
    const { message: m } = await original(to, "reply");
    const self = my.email.toLowerCase();
    const fromMe = m.fromEmail.toLowerCase() === self;
    const primary = fromMe ? m.to : [m.from && m.from !== m.fromEmail ? `${m.from} <${m.fromEmail}>` : m.fromEmail];
    const seen = new Set<string>([self]);
    const keep = (xs: string[]) => xs.filter((x) => { const e = emailOf(x); if (!e || seen.has(e)) return false; seen.add(e); return true; });
    const toList = keep(primary);
    const ccList = keep([...(opts.all ? [...(fromMe ? [] : m.to), ...m.cc] : []), ...addrList(opts.cc)]);
    const p = await prepare({
      to: toList.length ? toList : [my.email], cc: ccList, bcc: opts.bcc, subject: /^re:/i.test(m.subject) ? m.subject : `Re: ${m.subject}`,
      body, html: opts.html, attachments: opts.attachments, replyTo: to,
    }, "reply");
    return sendPrepared(p, "reply");
  }, (r) => ({ detail: `${r.to.concat(r.cc).join(", ")} · ${oneLine(r.subject, 50)}` }));
}

/** Forward a message (its body and attachments) with an optional note on top. Asks the person first, like send. */
function forward(msg: MessageRef, to: Addrs, note = "", opts: { cc?: Addrs; bcc?: Addrs } = {}): Promise<GmailDraft & { sent: true }> {
  return effect("net", "Forwarding an email", "Forwarded an email", typeof msg === "string" ? msg : msg.id, async () => {
    const { thread, message: m, html } = await original(msg, "forward");
    const files = await Promise.all(m.attachments.map((a) => attachment(thread.id, m.id, a.partId, { dir: join(HOME(), "gmail", "forward", m.id.replace(/[^\w.-]/g, "_")) })));
    const head = [`---------- Forwarded message ---------`, `From: ${m.from} <${m.fromEmail}>`, `Date: ${when(m.date)}`, `Subject: ${m.subject}`, `To: ${m.to.join(", ")}`, ...(m.cc.length ? [`Cc: ${m.cc.join(", ")}`] : [])];
    const p = await prepare({
      to, cc: opts.cc, bcc: opts.bcc, subject: /^fwd?:/i.test(m.subject) ? m.subject : `Fwd: ${m.subject}`, attachments: files,
      html: `${note ? textHtml(note) : ""}<br><div class="gmail_quote"><div dir="ltr" class="gmail_attr">${head.map(escapeHtml).join("<br>")}<br></div><br><br>${html || textHtml(m.text)}</div>`,
    }, "forward");
    p.preview = `${note}\n\n${head.join("\n")}\n\n${oneLine(m.text, 300)}`.trim();
    return sendPrepared(p, "forward");
  }, (r) => ({ detail: `${r.to.join(", ")} · ${oneLine(r.subject, 50)}` }));
}

// ---------------------------------------------------------------- label changes

/** Threads and the messages in them that `targets` name, with each thread's subject for the confirmation. */
async function resolveTargets(targets: Target | Target[], what: string): Promise<{ thread: string; subject: string; msgs: string[] }[]> {
  const list = (Array.isArray(targets) ? targets : [targets]).map((t) => {
    const id = typeof t === "string" ? t : t.id;
    const thread = typeof t === "string" ? t : t.threadId ?? t.id;
    if (!thread.startsWith("thread-")) throw new GmailError(`gmail.${what}(${JSON.stringify(id)}): needs a thread id or a row from gmail.search() / gmail.messages()`);
    return { thread, msg: id.startsWith("msg-") ? id : undefined };
  });
  if (!list.length) throw new GmailError(`gmail.${what}: no threads or messages given`);
  const raw = await fetchThreads([...new Set(list.map((t) => t.thread))]);
  const out = new Map<string, { thread: string; subject: string; msgs: Set<string> }>();
  for (const { thread, msg } of list) {
    const t = raw.find((x) => x[0] === thread)!;
    const ids = arr(arr(t[1])[1]).map((s) => str(arr(s)[0]));
    if (msg && !ids.includes(msg)) throw new GmailError(`gmail.${what}: ${msg} is not in ${thread}`);
    const e = out.get(thread) ?? { thread, subject: str(arr(arr(t[1])[0])[1]), msgs: new Set<string>() };
    for (const id of msg ? [msg] : ids) e.msgs.add(id);
    out.set(thread, e);
  }
  return [...out.values()].map((e) => ({ ...e, msgs: [...e.msgs] }));
}

/** A label name (Inbox, Starred, a user label) or id (^i, Label_12) to its id. */
async function labelId(name: string): Promise<string> {
  if (name.startsWith("^")) return name;
  const sys = Object.entries(SYSTEM_LABELS).find(([, [n]]) => n.toLowerCase() === name.toLowerCase());
  if (sys) return sys[0];
  const users = await userLabels();
  if (users.has(name)) return name;
  const user = [...users].find(([, n]) => n.toLowerCase() === name.toLowerCase());
  if (user) return user[0];
  throw new GmailError(`gmail.label: no label named ${JSON.stringify(name)} (labels: ${[...users.values()].join(", ") || "none of the account's own"}; system: ${Object.values(SYSTEM_LABELS).map(([n]) => n).join(", ")})`);
}

async function relabel(what: string, verb: string, targets: Target | Target[], add: string[], remove: string[], ask: boolean): Promise<{ threads: number; messages: number }> {
  const ts = await resolveTargets(targets, what);
  const messagesN = ts.reduce((n, t) => n + t.msgs.length, 0);
  if (ask) {
    const shown = ts.slice(0, 8).map((t) => `- ${oneLine(t.subject || t.thread, 70)}${t.msgs.length > 1 ? ` (${t.msgs.length} messages)` : ""}`);
    await approve(what, [`${verb} ${plural(ts.length, "Gmail thread")} (${plural(messagesN, "message")})?`, ...shown, ...(ts.length > 8 ? [`… and ${ts.length - 8} more`] : [])].join("\n"), "nothing was changed");
  }
  await mutate(what, ts.map((t) => ({ thread: t.thread, field: 7, payload: [add, remove, t.msgs] })));
  return { threads: ts.length, messages: messagesN };
}

const change = (r: { threads: number; messages: number }) => ({ detail: `${plural(r.threads, "thread")}, ${plural(r.messages, "message")}` });
const targetName = (t: Target | Target[]) => (Array.isArray(t) ? t : [t]).map((x) => (typeof x === "string" ? x : x.id)).join(" ");

/** Archive (take out of the Inbox). Asks the person first. */
function archive(targets: Target | Target[]) {
  return effect("write", "Archiving Gmail threads", "Archived Gmail threads", targetName(targets), () => relabel("archive", "Archive", targets, [], ["^i"], true), change);
}

/** Move to Trash. Asks the person first. */
function trash(targets: Target | Target[]) {
  return effect("delete", "Trashing Gmail threads", "Trashed Gmail threads", targetName(targets), () => relabel("trash", "Move to Trash", targets, ["^k"], ["^i"], true), change);
}

/** Add and remove labels by name ("Starred", "Important", "Inbox", a user label) or id. Asks the person first. */
function label(targets: Target | Target[], change_: { add?: string[]; remove?: string[] }) {
  return effect("write", "Relabelling Gmail threads", "Relabelled Gmail threads", targetName(targets), async () => {
    const [add, remove] = await Promise.all([Promise.all((change_.add ?? []).map(labelId)), Promise.all((change_.remove ?? []).map(labelId))]);
    if (!add.length && !remove.length) throw new GmailError("gmail.label: nothing to change (give add or remove)");
    const verb = [add.length ? `Add ${(change_.add ?? []).join(", ")}` : "", remove.length ? `${add.length ? "remove" : "Remove"} ${(change_.remove ?? []).join(", ")}` : ""].filter(Boolean).join(" and ");
    return relabel("label", `${verb} on`, targets, add, remove, true);
  }, change);
}

/** Mark read (or unread with read: false). No confirmation: it only changes the account's own read state. */
function markRead(targets: Target | Target[], read = true) {
  return effect("write", read ? "Marking Gmail read" : "Marking Gmail unread", read ? "Marked Gmail read" : "Marked Gmail unread", targetName(targets),
    () => relabel("markRead", "", targets, read ? [] : ["^u"], read ? ["^u"] : [], false), change);
}

export const gmail = { search, messages, read, labels, attachment, send, draft, deleteDraft, reply, forward, archive, trash, label, markRead, via: via_ };
