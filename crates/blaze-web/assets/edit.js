// The edit, `blaze why` drawn: every function the last build compiled again, under the edited
// function it was reached from through the calls, each with its cause.
import { causeText } from "./records.js";
import { crateColor, css } from "./palette.js";
import { lastName, shortName } from "./names.js";
import { el, fmtCount, fmtNs } from "./ui.js";

// Rebuilt names a function by crate and its own name (`main`, not `anim::main`). A name two
// functions share is settled by the one this build compiled (its nodeBuild); one still shared is
// shown as such, not picked.
export function analyseEdit(model, rebuilt, build) {
  const byName = new Map();
  const key = (krate, name) => krate + "\u0000" + lastName(name);
  const wanted = new Set(rebuilt.map((r) => key(r.krate, r.name)));
  for (let i = 0; i < model.n; i++) {
    const k = key(model.crates[model.krate[i]], model.names[i]);
    if (!wanted.has(k)) continue;
    (byName.get(k) ?? byName.set(k, []).get(k)).push(i);
  }
  const entries = rebuilt.map((r) => {
    let nodes = byName.get(key(r.krate, r.name)) ?? [];
    if (nodes.length > 1) {
      const thisBuild = nodes.filter((i) => model.nodeBuild[i] === build);
      if (thisBuild.length) nodes = thisBuild;
    }
    return { ...r, node: nodes.length === 1 ? nodes[0] : null, matches: nodes.length };
  });
  const inSet = new Map();
  for (const e of entries) if (e.node !== null) inSet.set(e.node, e);
  const roots = entries.filter((e) => e.node !== null && e.cause.type === "Edited").map((e) => e.node);
  // From the edited functions out to their callers, within what was compiled again: `via[c]` is
  // the function c calls on its way to an edited one, `link[c]` that call.
  const depth = new Map(roots.map((r) => [r, 0]));
  const via = new Map();
  const link = new Map();
  const levels = [];
  let frontier = [...new Set(roots)];
  if (frontier.length) levels.push({ nodes: frontier, links: [] });
  while (frontier.length) {
    const next = [];
    const links = [];
    for (const x of frontier) {
      for (const e of model.inn[x]) {
        const c = model.edges[2 * e];
        if (inSet.has(c) && !depth.has(c)) {
          depth.set(c, depth.get(x) + 1);
          via.set(c, x);
          link.set(c, e);
          next.push(c);
          links.push(e);
        }
      }
    }
    if (next.length) levels.push({ nodes: next, links });
    frontier = next;
  }
  return { entries, roots, depth, via, link, levels };
}

export class EditView {
  constructor(root, handlers) {
    this.root = root;
    this.handlers = handlers;
    this.root.replaceChildren(el("div", "note", "No build of an edit yet."));
  }

  render(model, a, unlisted, build) {
    const rows = [];
    const head = el("div", "note");
    head.textContent = a.entries.length
      ? `Build ${build} compiled ${fmtCount(a.entries.length)} again${unlisted ? ` (and ${fmtCount(unlisted)} more it did not list)` : ""}; ${fmtCount(a.roots.length)} in an edited file. Hover a row to see its path in the graph.`
      : "The last build compiled nothing again.";
    rows.push(head);

    const children = new Map();
    for (const [c, p] of a.via) (children.get(p) ?? children.set(p, []).get(p)).push(c);
    const entryOf = new Map(a.entries.filter((e) => e.node !== null).map((e) => [e.node, e]));
    const tree = el("div", "why-tree");
    const visit = (node, d) => {
      tree.appendChild(this.row(model, a, entryOf.get(node), node, d));
      for (const c of (children.get(node) ?? []).sort((x, y) => model.ns[y] - model.ns[x])) visit(c, d + 1);
    };
    for (const r of a.roots) visit(r, 0);
    if (a.roots.length) rows.push(tree);

    const unreached = a.entries.filter((e) => e.node !== null && !a.depth.has(e.node));
    if (unreached.length) {
      rows.push(el("h3", "", `Compiled again, with no call path to an edited function (${fmtCount(unreached.length)})`));
      const list = el("div", "why-tree");
      for (const e of unreached.slice(0, 500)) list.appendChild(this.row(model, a, e, e.node, 0));
      if (unreached.length > 500) list.appendChild(el("div", "note", `and ${fmtCount(unreached.length - 500)} more`));
      rows.push(list);
    }
    const unknown = a.entries.filter((e) => e.node === null);
    if (unknown.length) {
      rows.push(el("h3", "", `Not one function of the graph (${fmtCount(unknown.length)})`));
      const list = el("div", "why-tree");
      for (const e of unknown.slice(0, 200)) {
        const r = el("div", "why-row unknown");
        r.append(el("span", "why-name", e.name), el("span", "why-crate", e.krate), el("span", "why-cause", causeText(e.cause)),
          el("span", "why-time", e.matches ? `${e.matches} functions have this name` : "no function has this name"));
        list.appendChild(r);
      }
      if (unknown.length > 200) list.appendChild(el("div", "note", `and ${fmtCount(unknown.length - 200)} more`));
      rows.push(list);
    }
    this.root.replaceChildren(...rows);
  }

  row(model, a, entry, node, d) {
    const r = el("div", "why-row");
    r.style.paddingLeft = `${8 + d * 16}px`;
    const dot = el("span", "dot");
    dot.style.background = css(crateColor(model.crates[model.krate[node]]));
    const name = el("span", "why-name", d ? `↳ ${shortName(model.names[node])}` : model.names[node]);
    name.title = model.names[node];
    r.append(dot, name, el("span", "why-cause", entry ? causeText(entry.cause) : ""), el("span", "why-time", fmtNs(model.ns[node])));
    const path = () => {
      const nodes = [node];
      const links = [];
      let x = node;
      while (a.via.has(x)) { links.push(a.link.get(x)); x = a.via.get(x); nodes.push(x); }
      return { nodes, links };
    };
    r.addEventListener("mouseenter", () => this.handlers.onHoverPath?.(path()));
    r.addEventListener("mouseleave", () => this.handlers.onHoverPath?.(null));
    r.addEventListener("click", () => this.handlers.onSelect?.(node));
    return r;
  }
}
