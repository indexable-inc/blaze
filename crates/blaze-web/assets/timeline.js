// The build's steps on a timeline (Started/Ended), the crates under them (Crate records), and
// what the build said (Said, Refused).
import { crateColor, css } from "./palette.js";
import { STEP_WORD, STEP_WHOLE } from "./records.js";
import { el, fitCanvas, fmtNs } from "./ui.js";

const STEP_ORDER = [
  "LoadWorkspace", "Fetch", "Remote", "BuildScripts", "Macros", "Resolve", "FrontEnd", "Std", "Codegen", "Optimize",
  "Link", "Store", "Check", "Run", "Test",
];
const STEP_COLOR = {
  LoadWorkspace: "#6e7681", Fetch: "#8b949e", Remote: "#8b949e", BuildScripts: "#bc8cff", Macros: "#bc8cff",
  Resolve: "#a371f7", FrontEnd: "#58a6ff", Std: "#56d4dd", Codegen: "#f0b72f", Optimize: "#d29922",
  Link: "#3fb950", Store: "#6e7681", Check: "#58a6ff", Run: "#db61a2", Test: "#db61a2",
};
const LANE = 16;
const LEFT = 92;

export class Timeline {
  constructor(root, tooltip) {
    this.root = root;
    this.tooltip = tooltip;
    this.head = el("div", "note");
    this.canvas = el("canvas", "timeline");
    this.log = el("div", "log");
    root.replaceChildren(this.head, this.canvas, el("h3", "", "What the build said"), this.log);
    this.canvas.addEventListener("mousemove", (e) => this.hover(e));
    this.canvas.addEventListener("mouseleave", () => this.tooltip.hide());
    new ResizeObserver(() => this.visible && this.draw()).observe(root);
    this.reset(null);
  }

  reset(began) {
    this.began = began;
    this.bars = [];
    this.open = new Map();
    this.crates = new Map();
    this.now = 0;
    this.finished = null;
    this.log.replaceChildren();
    this.schedule();
  }

  record(r) {
    const ev = r.event;
    this.now = Math.max(this.now, r.at_ns);
    switch (ev.type) {
      case "Started": {
        const k = `${ev.step}\u0000${ev.of ?? ""}`;
        const bar = { step: ev.step, of: ev.of, start: r.at_ns, end: null };
        this.bars.push(bar);
        (this.open.get(k) ?? this.open.set(k, []).get(k)).push(bar);
        break;
      }
      case "Ended": {
        const k = `${ev.step}\u0000${ev.of ?? ""}`;
        const stack = this.open.get(k);
        const bar = stack?.pop();
        if (bar) bar.end = r.at_ns;
        else this.say("Warning", `Ended ${ev.step}${ev.of ? ` of ${ev.of}` : ""} at ${fmtNs(r.at_ns)} with no Started before it`);
        break;
      }
      case "Crate": {
        let c = this.crates.get(ev.name);
        if (!c) this.crates.set(ev.name, (c = { name: ev.name, start: r.at_ns, end: null, state: ev.state, fns: ev.fns }));
        c.state = ev.state;
        c.fns = ev.fns;
        if (ev.state === "Waiting") c.start = r.at_ns;
        if (ev.state === "Running" && c.runAt === undefined) c.runAt = r.at_ns;
        if (ev.state === "Built" || ev.state === "UpToDate" || ev.state === "Failed") c.end = r.at_ns;
        break;
      }
      case "Said": this.say(ev.level, ev.text, r.at_ns); break;
      case "Refused": this.say("Error", `refused ${ev.name}: ${ev.why}`, r.at_ns); break;
      case "Finished":
        this.finished = { at: r.at_ns, ok: ev.ok, summary: ev.summary };
        if (ev.summary.error) this.say("Error", ev.summary.error, r.at_ns);
        break;
    }
    this.schedule();
  }

  say(level, text, at) {
    const row = el("div", `log-row ${level.toLowerCase()}`);
    row.append(el("span", "log-at", at === undefined ? "" : fmtNs(at)), el("span", "log-level", level.toLowerCase()), el("span", "log-text", text));
    this.log.appendChild(row);
  }

  // The step under way: the last Started not yet Ended.
  current() {
    for (let i = this.bars.length - 1; i >= 0; i--) {
      const b = this.bars[i];
      if (b.end === null) return `${STEP_WORD[b.step]} ${b.of ?? STEP_WHOLE[b.step]}`;
    }
    return this.finished ? (this.finished.ok ? "finished" : "failed") : null;
  }

  show() { this.visible = true; this.draw(); }
  hide() { this.visible = false; }

  schedule() {
    if (!this.visible || this.pending) return;
    this.pending = requestAnimationFrame(() => { this.pending = 0; this.draw(); });
  }

  draw() {
    const span = Math.max(this.finished?.at ?? this.now, 1);
    const w = Math.max(200, this.root.clientWidth - 4);
    // Overlapping bars of a step share its row in lanes.
    const rows = [];
    for (const step of STEP_ORDER) {
      const bars = this.bars.filter((b) => b.step === step);
      if (!bars.length) continue;
      const lanes = [];
      for (const b of bars) {
        const end = b.end ?? span;
        let l = lanes.findIndex((t) => t <= b.start);
        if (l < 0) { l = lanes.length; lanes.push(0); }
        lanes[l] = end;
        b.lane = l;
      }
      rows.push({ step, bars, lanes: lanes.length });
    }
    const crates = [...this.crates.values()].sort((a, b) => (a.runAt ?? a.start) - (b.runAt ?? b.start));
    const stepsH = rows.reduce((s, r) => s + r.lanes * LANE + 6, 0);
    const h = 26 + stepsH + (crates.length ? 18 + crates.length * 11 : 0) + 8;
    const ctx = fitCanvas(this.canvas, w, h);
    ctx.fillStyle = "#0b0f14";
    ctx.fillRect(0, 0, w, h);
    const X = (t) => LEFT + ((w - LEFT - 10) * t) / span;
    ctx.font = "10px ui-sans-serif, system-ui, sans-serif";
    ctx.textBaseline = "middle";
    ctx.strokeStyle = "#21262d";
    ctx.fillStyle = "#6e7681";
    const tick = niceTick(span / 6);
    for (let t = 0; t <= span; t += tick) {
      ctx.beginPath();
      ctx.moveTo(X(t) + 0.5, 16);
      ctx.lineTo(X(t) + 0.5, h);
      ctx.stroke();
      ctx.fillText(fmtNs(t), X(t) + 2, 8);
    }
    this.hits = [];
    let y = 22;
    for (const r of rows) {
      ctx.fillStyle = "#8b949e";
      ctx.fillText(STEP_WORD[r.step] + (r.step === "Std" ? " std" : ""), 6, y + (r.lanes * LANE) / 2);
      for (const b of r.bars) {
        const x0 = X(b.start), x1 = Math.max(x0 + 2, X(b.end ?? span));
        const by = y + b.lane * LANE;
        ctx.fillStyle = STEP_COLOR[b.step];
        ctx.globalAlpha = b.end === null ? 0.5 : 0.85;
        ctx.fillRect(x0, by + 2, x1 - x0, LANE - 4);
        ctx.globalAlpha = 1;
        if (x1 - x0 > 40 && b.of) {
          ctx.fillStyle = "#0b0f14";
          ctx.fillText(b.of.slice(0, Math.floor((x1 - x0 - 6) / 5.5)), x0 + 3, by + LANE / 2);
        }
        this.hits.push({ x0, x1, y0: by, y1: by + LANE, lines: [`${STEP_WORD[b.step]} ${b.of ?? STEP_WHOLE[b.step]}`, b.end === null ? `since ${fmtNs(b.start)}, under way` : `${fmtNs(b.start)} to ${fmtNs(b.end)}: ${fmtNs(b.end - b.start)}`] });
      }
      y += r.lanes * LANE + 6;
    }
    if (crates.length) {
      ctx.fillStyle = "#8b949e";
      ctx.fillText("crates", 6, y + 8);
      y += 18;
      for (const c of crates) {
        const s = c.runAt ?? c.start;
        const x0 = X(s), x1 = Math.max(x0 + 2, X(c.end ?? span));
        ctx.fillStyle = c.state === "Failed" ? "#f85149" : css(crateColor(c.name), c.state === "UpToDate" ? 0.35 : 0.85);
        ctx.fillRect(x0, y + 1, x1 - x0, 8);
        this.hits.push({ x0, x1, y0: y, y1: y + 10, lines: [c.name, `${c.state}, ${c.fns.done} of ${c.fns.total} functions done, ${c.fns.kept} kept`, `${fmtNs(s)} to ${c.end === null ? "now" : fmtNs(c.end)}`] });
        y += 11;
      }
    }
    const done = this.finished ? `${this.finished.ok ? "Finished" : "Failed"} in ${fmtNs(this.finished.summary.took_ns)}.` : this.bars.length ? "Under way." : "No steps yet.";
    this.head.textContent = this.began ? `Build ${this.began.build}: blaze ${this.began.command}. ${done}` : "No build records yet.";
  }

  hover(e) {
    const r = this.canvas.getBoundingClientRect();
    const x = e.clientX - r.left, y = e.clientY - r.top;
    const h = this.hits?.find((h) => x >= h.x0 && x <= h.x1 && y >= h.y0 && y <= h.y1);
    if (h) this.tooltip.show(e.clientX, e.clientY, h.lines);
    else this.tooltip.hide();
  }
}

function niceTick(x) {
  const p = 10 ** Math.floor(Math.log10(x));
  for (const k of [1, 2, 5, 10]) if (k * p >= x) return k * p;
  return 10 * p;
}
