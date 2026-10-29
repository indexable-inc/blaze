// The build page (docs/web.md). It reads the websocket beside it, or with ?fixture=<file> a
// snapshot (and with &records=<file> a CBOR sequence of records) from this directory.
import { decode, decodeSequence } from "./cbor.js";
import { parseSnapshot, isSnapshot } from "./snapshot.js";
import { parseRecord } from "./records.js";
import { Model } from "./model.js";
import { MainView, webgl2Missing } from "./mainview.js";
import { CrateDag } from "./dag.js";
import { Search } from "./search.js";
import { Treemap } from "./treemap.js";
import { Timeline } from "./timeline.js";
import { EditView, analyseEdit } from "./edit.js";
import { Tooltip, el, fmtCount, fmtNs } from "./ui.js";
import { HOW } from "./snapshot.js";

const $ = (id) => document.getElementById(id);
const params = new URLSearchParams(location.search);
const slow = Number(params.get("slow") ?? 1);
const FLUSH_MS = 300;
const SPREAD_MAX = 2000;

const state = {
  model: null, view: null, began: null, rebuilt: [], unlisted: 0, analysis: null,
  pendingFlash: new Set(), flushTimer: 0, spreadBuild: null, litThisBuild: 0, dead: false,
  connection: "connecting", ws: null, mouse: [0, 0], snapshots: 0, records: 0,
};
const tooltip = new Tooltip();
let dag, search, treemap, timeline, editView;

// ------------------------------------------------------------------ errors
function fatal(title, detail) {
  if (state.dead) return;
  state.dead = true;
  $("error-title").textContent = title;
  $("error-detail").textContent = detail;
  $("error").hidden = false;
  if (state.ws && state.ws.readyState <= 1) state.ws.close(4000, "the page refused what it was sent");
  setConnection("stopped");
}

window.addEventListener("error", (e) => {
  // The browser reports a resize that took two frames to settle as an error with no error.
  if (!e.error && /^ResizeObserver loop/.test(e.message)) return;
  fatal("The page failed", String(e.error?.stack ?? e.message));
});
window.addEventListener("unhandledrejection", (e) => fatal("The page failed", String(e.reason?.stack ?? e.reason)));

// ------------------------------------------------------------------ status line
function setConnection(s) {
  state.connection = s;
  const c = $("st-conn");
  c.textContent = s;
  c.className = `conn ${s.split(" ")[0]}`;
}

let statusFrame = 0;
function status() {
  if (statusFrame) return;
  statusFrame = requestAnimationFrame(() => {
    statusFrame = 0;
    const m = state.model;
    if (!m) return;
    const c = m.counts();
    $("st-build").textContent = `build ${state.began?.build ?? m.build}`;
    $("st-root").textContent = state.began?.root ?? m.root;
    $("st-root").title = state.began?.root ?? m.root;
    $("st-step").textContent = timeline.current() ?? "idle";
    $("st-counts").replaceChildren(
      count("compiled", c.compiled + c.unit, "compiled"), count("unchanged", c.unchanged, "unchanged"), count("stored", c.stored, "from the store"),
      ...(state.litThisBuild ? [count("lit", state.litThisBuild, "this build")] : []),
      count("calls", m.m, "calls"),
    );
  });
}

function count(cls, n, label) {
  const s = el("span", `count ${cls}`);
  s.append(el("b", "", fmtCount(n)), document.createTextNode(` ${label}`));
  return s;
}

// ------------------------------------------------------------------ snapshot and records
function onSnapshot(bytes) {
  let snap;
  try {
    snap = parseSnapshot(bytes);
  } catch (e) {
    fatal("The snapshot is malformed", e.message);
    return;
  }
  state.model = new Model(snap);
  state.began = null;
  state.rebuilt = [];
  state.unlisted = 0;
  state.analysis = null;
  state.spreadBuild = null;
  state.litThisBuild = 0;
  state.pendingFlash.clear();
  const host = $("graph-canvas");
  if (state.view) {
    state.view.graph.destroy();
    host.replaceChildren();
  }
  state.view = new MainView(host, state.model, {
    onSelect: (i) => select(i, true),
    onHover: (i, ev) => hoverPoint(i, ev),
    onLayout: (running) => { $("btn-layout").textContent = running ? "Pause layout" : "Run layout"; },
  }, slow);
  state.view.graph.ready.catch((e) => fatal("cosmos.gl could not start WebGL2", String(e?.stack ?? e)));
  dag.setModel(state.model);
  search.setModel(state.model);
  treemap.setModel(state.model);
  timeline.reset(null);
  editView.root.replaceChildren(el("div", "note", "No functions compiled again in the last build."));
  showTab(currentTab);
  status();
  state.snapshots++;
}

function onRecordBytes(bytes) {
  let r;
  try {
    r = parseRecord(decode(bytes));
  } catch (e) {
    fatal("A record is malformed", e.message);
    return;
  }
  onRecord(r);
}

function onRecord(r) {
  if (state.dead) return;
  if (!state.model) {
    fatal("A record came before the snapshot", JSON.stringify(r.event.type));
    return;
  }
  state.records++;
  const ev = r.event;
  switch (ev.type) {
    case "Began":
      state.began = ev;
      state.rebuilt = [];
      state.unlisted = 0;
      state.analysis = null;
      state.litThisBuild = 0;
      state.model.current = ev.build;
      state.view.clearSpread();
      state.view.restyleAll();
      timeline.reset(ev);
      break;
    case "Functions": {
      const build = state.began?.build ?? state.model.build;
      let res;
      try {
        res = state.model.mergeFns(ev.object, ev.functions, build);
      } catch (e) {
        fatal("A Functions record does not fit the graph", e.message);
        return;
      }
      if (res.added.length || res.newEdges) {
        state.view.addNodes(res.added, res.newEdges);
        dag.invalidate();
        search.invalidate();
      } else {
        state.view.restyleAll();
      }
      treemap.invalidate();
      dag.invalidate();
      state.litThisBuild += res.touched.length;
      for (const i of res.touched) state.pendingFlash.add(i);
      scheduleFlush();
      break;
    }
    case "Rebuilt":
      state.rebuilt.push(ev);
      scheduleFlush();
      break;
    case "Unlisted":
      state.unlisted += ev.count;
      break;
    case "Finished":
      scheduleFlush(0);
      break;
  }
  timeline.record(r);
  status();
}

// Functions and Rebuilt come in bursts: the glow and the edit's spread are worked out once a burst
// has gone quiet, or every FLUSH_MS while one goes on.
function scheduleFlush(ms = FLUSH_MS) {
  if (state.flushTimer && ms) return;
  clearTimeout(state.flushTimer);
  state.flushTimer = setTimeout(flush, ms);
}

function flush() {
  state.flushTimer = 0;
  const build = state.began?.build ?? state.model.build;
  if (state.rebuilt.length) {
    state.analysis = analyseEdit(state.model, state.rebuilt, build);
    editView.render(state.model, state.analysis, state.unlisted, build);
  }
  const a = state.analysis;
  const pending = state.pendingFlash;
  state.pendingFlash = new Set();
  if (a && a.roots.length && a.entries.length <= SPREAD_MAX && state.spreadBuild !== build) {
    state.spreadBuild = build;
    state.view.spread(a.levels, $("follow").checked);
    for (const l of a.levels) for (const i of l.nodes) pending.delete(i);
  }
  if (pending.size) state.view.flash(pending);
}

// ------------------------------------------------------------------ selection and hover
let selected = null;
function select(i, fromGraph) {
  selected = i;
  if (i === null) {
    search.select(null);
    state.view.highlight(null);
    return;
  }
  if (fromGraph || currentTab !== "edit") showTab("search");
  search.select(i);
  if (!fromGraph) state.view.centreOn(i, state.view.selection?.nodes);
}

function hoverPoint(i, ev) {
  if (i === null || i === undefined) {
    tooltip.hide();
    return;
  }
  const m = state.model;
  const x = ev?.clientX ?? state.mouse[0], y = ev?.clientY ?? state.mouse[1];
  tooltip.show(x, y, [m.names[i], `${m.crates[m.krate[i]]} · ${m.ns[i] ? fmtNs(m.ns[i]) : HOW[m.how[i]].toLowerCase()} · build ${m.nodeBuild[i]}`]);
}

// ------------------------------------------------------------------ tabs and layout
let currentTab = "crates";
const TABS = ["crates", "search", "time", "phases", "edit"];
function showTab(name) {
  currentTab = name;
  for (const t of TABS) {
    $(`tab-${t}`).hidden = t !== name;
    $(`tabbtn-${t}`).classList.toggle("on", t === name);
  }
  if (name === "time") treemap.show(); else treemap.hide();
  if (name === "phases") timeline.show(); else timeline.hide();
  if (name === "crates" && state.model) dag.show(); else dag.hide();
}

function splitter() {
  const s = $("splitter"), panel = $("panel");
  s.addEventListener("pointerdown", (e) => {
    s.setPointerCapture(e.pointerId);
    const move = (ev) => {
      const w = Math.min(window.innerWidth - 320, Math.max(320, window.innerWidth - ev.clientX));
      panel.style.width = `${w}px`;
    };
    const up = () => { s.removeEventListener("pointermove", move); s.removeEventListener("pointerup", up); };
    s.addEventListener("pointermove", move);
    s.addEventListener("pointerup", up);
  });
}

function init() {
  const missing = webgl2Missing();
  if (missing) {
    fatal("No WebGL2", missing);
    return;
  }
  const handlers = {
    onSelect: (i) => select(i, false),
    onHover: (i) => state.view?.graph.setHostHoveredPoint(i ?? undefined),
  };
  dag = new CrateDag($("tab-crates"), null, {
    ...handlers,
    onHoverUnit: (nodes) => state.view?.preview(nodes ? { nodes } : null),
    onFocusUnit: (nodes) => state.view?.fit(nodes),
  });
  search = new Search($("tab-search"), { n: 0, names: [] }, { ...handlers, onNeighbourhood: (sel) => state.view?.highlight(sel) });
  treemap = new Treemap($("tab-time"), { n: 0, crates: [], modules: [], names: [] }, { ...handlers, onHoverNodes: () => {} }, tooltip);
  timeline = new Timeline($("tab-phases"), tooltip);
  editView = new EditView($("tab-edit"), { ...handlers, onHoverPath: (p) => state.view?.preview(p ? { nodes: p.nodes, links: p.links, focus: p.nodes[0] } : null) });
  for (const t of TABS) $(`tabbtn-${t}`).addEventListener("click", () => showTab(t));
  $("btn-fit").addEventListener("click", () => state.view?.fit());
  $("btn-layout").addEventListener("click", () => {
    if (!state.view) return;
    const on = !state.view.layoutRunning;
    state.view.setLayoutRunning(on);
    $("btn-layout").textContent = on ? "Pause layout" : "Run layout";
  });
  $("graph").addEventListener("pointermove", (e) => { state.mouse = [e.clientX, e.clientY]; });
  document.addEventListener("keydown", (e) => {
    if (e.key === "/" && document.activeElement !== search.input) {
      e.preventDefault();
      showTab("search");
      search.input.focus();
    } else if (e.key === "Escape") select(null);
  });
  splitter();
  showTab("crates");

  const fixture = params.get("fixture");
  if (params.has("slow") && (fixture === null || !(slow >= 1 && slow <= 100))) {
    fatal("Bad address", "slow=<1 to 100> slows the animations of a fixture (index.html?fixture=<file>), for recording them.");
    return;
  }
  if (fixture !== null) runFixture(fixture, params.get("records"));
  else connect();
}

// ------------------------------------------------------------------ sources
function connect() {
  const u = new URL("ws", location.href);
  u.protocol = u.protocol === "https:" ? "wss:" : "ws:";
  u.search = "";
  const ws = new WebSocket(u);
  state.ws = ws;
  ws.binaryType = "arraybuffer";
  let first = true;
  // A snapshot is unzipped asynchronously, and the records after it must wait for it.
  let queue = Promise.resolve();
  ws.onopen = () => setConnection("waiting for the snapshot");
  ws.onmessage = (e) => {
    if (state.dead) return;
    if (typeof e.data === "string") {
      fatal("The server sent text", "Every message of the build's websocket is binary (docs/web.md, \"The wire\").");
      return;
    }
    const bytes = new Uint8Array(e.data);
    // A record is a CBOR map, so a message starting with gzip's 1f 8b is a snapshot.
    const zipped = bytes.length >= 2 && bytes[0] === 0x1f && bytes[1] === 0x8b;
    if (first && !zipped) {
      fatal("The first message is not a snapshot", `It starts with ${[...bytes.subarray(0, 8)].map((b) => b.toString(16).padStart(2, "0")).join(" ")}, not gzip's 1f 8b.`);
      return;
    }
    first = false;
    queue = queue.then(async () => {
      if (state.dead) return;
      if (!zipped) {
        onRecordBytes(bytes);
        return;
      }
      let snapshot;
      try {
        snapshot = await gunzip(bytes);
      } catch (err) {
        fatal("The snapshot does not unzip", String(err));
        return;
      }
      if (!isSnapshot(snapshot)) {
        fatal("The snapshot is not one", 'It does not start with "BLZW" (docs/web.md, "The snapshot").');
        return;
      }
      onSnapshot(snapshot);
      setConnection("live");
    });
  };
  ws.onclose = (e) => {
    if (!state.dead) setConnection(`closed${e.code ? ` (${e.code}${e.reason ? `: ${e.reason}` : ""})` : ""}`);
  };
  ws.onerror = () => {
    if (!state.dead) setConnection("closed (error)");
  };
}

async function gunzip(bytes) {
  const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream("gzip"));
  return new Uint8Array(await new Response(stream).arrayBuffer());
}

async function fetchHere(name, what) {
  if (!/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(name)) throw new Error(`${what} ${JSON.stringify(name)} is not a file name of this directory`);
  const res = await fetch(new URL(name, location.href));
  if (!res.ok) throw new Error(`${what} ${name}: HTTP ${res.status}`);
  return new Uint8Array(await res.arrayBuffer());
}

async function runFixture(name, recordsName) {
  setConnection("fixture");
  let snap, records = [];
  try {
    snap = await fetchHere(name, "the fixture");
    if (recordsName) {
      const items = decodeSequence(await fetchHere(recordsName, "the records"));
      records = items.map((v, i) => {
        try {
          return parseRecord(v);
        } catch (e) {
          throw new Error(`item ${i}: ${e.message}`);
        }
      });
    }
  } catch (e) {
    fatal("The fixture could not be read", e.message);
    return;
  }
  onSnapshot(snap);
  window.blazePage.loaded = true;
  if (records.length) {
    const delay = Number(params.get("delay") ?? 1500);
    window.blazePage.feed = () => { for (const r of records) onRecord(r); window.blazePage.fed = performance.now(); };
    if (delay >= 0) setTimeout(window.blazePage.feed, delay);
  }
}

window.blazePage = {
  loaded: false, fed: null,
  get animating() { return state.view?.animating ?? false; },
  get layoutRunning() { return state.view?.layoutRunning ?? false; },
  get dead() { return state.dead; },
  get snapshots() { return state.snapshots; },
  get records() { return state.records; },
  get nodes() { return state.model?.n ?? 0; },
  get connection() { return state.connection; },
  showTab, select: (i) => select(i, false),
  find: (name) => state.model?.names.indexOf(name) ?? -1,
};

init();
