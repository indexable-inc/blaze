// Search by `crate::path::fn` (substring, then in-order letters), and the selected function's
// callers and callees a few calls out.
import { crateColor, css } from "./palette.js";
import { HOW } from "./snapshot.js";
import { el, fmtCount, fmtNs } from "./ui.js";

const RESULTS = 80;
const LIST = 300;

export class Search {
  constructor(root, model, handlers) {
    this.root = root;
    this.handlers = handlers;
    this.hops = 1;
    this.input = el("input", "search-input");
    this.input.type = "search";
    this.input.placeholder = "crate::path::fn";
    this.input.spellcheck = false;
    this.results = el("div", "results");
    this.detail = el("div", "detail");
    root.replaceChildren(this.input, this.results, this.detail);
    this.input.addEventListener("input", () => this.query());
    this.input.addEventListener("keydown", (e) => {
      if (e.key === "Enter" && this.top !== undefined) this.handlers.onSelect?.(this.top);
    });
    this.setModel(model);
  }

  setModel(model) {
    this.model = model;
    this.lower = null;
    this.selected = null;
    this.detail.replaceChildren();
    this.query();
  }

  invalidate() {
    this.lower = null;
  }

  query() {
    const q = this.input.value.trim().toLowerCase();
    this.top = undefined;
    if (!q) {
      this.results.replaceChildren(el("div", "note", `${fmtCount(this.model.n)} functions. Type part of a path; Enter selects the first.`));
      return;
    }
    const m = this.model;
    if (!this.lower || this.lower.length !== m.n) this.lower = m.names.slice(0, m.n).map((s) => s.toLowerCase());
    const hits = [];
    for (let i = 0; i < m.n; i++) {
      const s = this.lower[i];
      const at = s.indexOf(q);
      if (at < 0) continue;
      let score = 3 + at / 1000;
      if (s === q) score = 0;
      else if (s.endsWith("::" + q)) score = 1;
      else if (s.lastIndexOf("::") < at) score = 2 + at / 1000;
      hits.push([score + s.length / 1e6, i]);
    }
    let fuzzy = 0;
    if (hits.length < RESULTS) {
      const have = new Set(hits.map((h) => h[1]));
      for (let i = 0; i < m.n; i++) {
        if (have.has(i)) continue;
        const g = gaps(this.lower[i], q);
        if (g >= 0) { hits.push([10 + g / 100 + this.lower[i].length / 1e6, i]); fuzzy++; }
      }
    }
    hits.sort((a, b) => a[0] - b[0]);
    this.top = hits[0]?.[1];
    const shown = hits.slice(0, RESULTS).map(([, i]) => this.item(i));
    const note = el("div", "note", hits.length > RESULTS ? `${fmtCount(hits.length)} match; the first ${RESULTS}.` : `${fmtCount(hits.length)} match${fuzzy ? `, ${fuzzy} by letters in order` : ""}.`);
    this.results.replaceChildren(note, ...shown);
  }

  item(i, extra) {
    const m = this.model;
    const r = el("div", "fn-row");
    const dot = el("span", "dot");
    dot.style.background = css(crateColor(m.crates[m.krate[i]]));
    const name = el("span", "fn-name", m.names[i]);
    name.title = m.names[i];
    r.append(dot, name);
    if (extra) r.append(el("span", "fn-depth", extra));
    r.append(el("span", "fn-time", m.ns[i] ? fmtNs(m.ns[i]) : HOW[m.how[i]].toLowerCase()));
    r.addEventListener("click", () => this.handlers.onSelect?.(i));
    r.addEventListener("mouseenter", () => this.handlers.onHover?.(i));
    r.addEventListener("mouseleave", () => this.handlers.onHover?.(null));
    return r;
  }

  select(i) {
    this.selected = i;
    this.renderDetail();
  }

  renderDetail() {
    const i = this.selected;
    if (i === null || i === undefined) {
      this.detail.replaceChildren();
      this.handlers.onNeighbourhood?.(null);
      return;
    }
    const m = this.model;
    const callers = m.neighbourhood(i, this.hops, "callers");
    const callees = m.neighbourhood(i, this.hops, "callees");
    const head = el("div", "detail-head");
    head.append(el("div", "detail-name", m.names[i]));
    const meta = el("div", "detail-meta");
    meta.append(
      el("span", "", m.crates[m.krate[i]]), el("span", "", m.modules[m.group[i]]),
      el("span", "", m.ns[i] ? `compiled in ${fmtNs(m.ns[i])}` : HOW[m.how[i]].toLowerCase()), el("span", "", `build ${m.nodeBuild[i]}`),
    );
    head.append(meta);
    const hops = el("div", "segmented");
    hops.append(el("span", "seg-label", "calls out"));
    for (const n of [1, 2, 3]) {
      const b = el("button", n === this.hops ? "seg on" : "seg", String(n));
      b.addEventListener("click", () => { this.hops = n; this.renderDetail(); });
      hops.append(b);
    }
    const list = (title, items) => {
      const box = el("div", "nbr");
      box.append(el("h3", "", `${title} (${fmtCount(items.length)})`));
      for (const it of items.slice(0, LIST)) box.append(this.item(it.node, this.hops > 1 ? `${it.depth}` : undefined));
      if (items.length > LIST) box.append(el("div", "note", `and ${fmtCount(items.length - LIST)} more`));
      return box;
    };
    this.detail.replaceChildren(head, hops, list("Callers", callers), list("Callees", callees));
    this.handlers.onNeighbourhood?.({
      focus: i,
      nodes: [i, ...callers.map((x) => x.node), ...callees.map((x) => x.node)],
      links: [...callers, ...callees].map((x) => x.edge),
    });
  }
}

// The letters of q in s in order: the number of letters skipped, or -1.
function gaps(s, q) {
  let j = 0, g = 0, last = -1;
  for (let i = 0; i < s.length && j < q.length; i++) {
    if (s[i] === q[j]) {
      if (last >= 0) g += i - last - 1;
      last = i;
      j++;
    }
  }
  return j === q.length ? g : -1;
}
