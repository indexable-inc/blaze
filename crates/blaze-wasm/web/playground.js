// What the playground (`dioxus/playground`, a Dioxus app) leaves to JavaScript: running the compiler
// in `worker.js` on every edit, and the programs it builds. The app calls `window.blaze`,
// and hears how builds go through `blaze_state` and `blaze_line`.

import init, { blaze_line, blaze_state } from "./pkg/dioxus/playground.js";
import { Program } from "./program.js";

const ms = (value) => `${value.toFixed(1)} ms`;
const mib = (bytes) => `${(bytes / 2 ** 20).toFixed(1)} MiB`;
const kib = (bytes) => (bytes < 10240 ? `${bytes} B` : `${(bytes / 1024).toFixed(0)} KiB`);
const count = (n, what) => `${n} ${what}${n === 1 ? "" : "s"}`;

// The one shown, of the two below.
let shown = null;

// The builds of one compiler: one at a time, an edit made during a build is built after it.
class Builds {
  constructor(worker, delay) {
    this.worker = new Worker(worker, { type: "module" });
    this.worker.onerror = (event) => this.fail("the compiler failed to load", event.message);
    this.worker.onmessage = ({ data }) => this.message(data);
    this.delay = delay;
    this.kind = "building";
    this.text = "";
    this.sent = 0;
    this.edited = 0;
    this.building = false;
    this.ready = false;
    this.started = 0;
  }

  // `building`, `running` or `error`: said to the app when this is the one shown, and
  // again when it is shown again.
  state(kind) {
    this.kind = kind;
    if (shown === this) {
      blaze_state(kind);
    }
  }

  // A line of the builds the app lists: `level` is `info`, `good` or `bad`, `parts`
  // are measurements and `notes` longer text. What a compiler not shown says is dropped.
  line(level, head, parts = [], notes = []) {
    if (shown === this) {
      blaze_line(new Date().toLocaleTimeString("en-GB"), level, head, parts, notes);
    }
  }

  fail(head, error) {
    this.line("bad", head, [], [String(error)]);
    this.state("error");
  }

  edit(text) {
    this.text = text;
    this.edited += 1;
    clearTimeout(this.timer);
    this.timer = setTimeout(() => this.build(), this.delay);
  }

  build() {
    if (!this.ready || this.building || this.sent === this.edited) {
      return;
    }
    this.building = true;
    this.sent = this.edited;
    this.worker.postMessage({ id: this.sent, text: this.text, ...this.options() });
    this.started = performance.now();
  }

  message(data) {
    if ("progress" in data) {
      return;
    }
    if (data.loaded) {
      this.line("info", "compiler ready", this.describe(data.loaded));
      this.ready = true;
      this.build();
      return;
    }
    this.building = false;
    if (data.error) {
      this.fail("build failed", data.error);
    } else {
      try {
        const applied = this.apply(data);
        if (applied !== null) {
          const { report, module } = data;
          const { parts, notes } = applied;
          const failed = notes.length > 0;
          this.line(failed ? "bad" : "good", `${failed ? "built in part" : "built"} in ${ms(performance.now() - this.started)}`, [
            `${count(report.functions.length, "function")} swapped`,
            `${kib(module.length)} module`,
            ...parts,
            `codegen ${ms(report.codegen_ms)}`,
            `link ${ms(report.link_ms)}`,
            `compiler memory ${mib(report.compiler_bytes)}`,
          ], notes);
          this.state(failed ? "error" : "running");
        }
      } catch (error) {
        this.fail("the build did not load", error.stack ?? error);
      }
    }
    this.build();
  }
}

const errors = (report) => report.errors ?? [];

// The programs that draw: `core` and the program are all there is, compiled from source.
class Canvas extends Builds {
  constructor() {
    super("worker.js", 60);
    this.canvas = document.getElementById("canvas");
    this.context = this.canvas.getContext("2d");
    const context = this.context;
    this.host = {
      fill: (red, green, blue) => (context.fillStyle = `rgb(${red} ${green} ${blue})`),
      rect: (x, y, width, height) => context.fillRect(x, y, width, height),
      circle(x, y, radius) {
        context.beginPath();
        context.arc(x, y, radius, 0, 2 * Math.PI);
        context.fill();
      },
      text: (pointer, length, x, y) => context.fillText(this.program.text(pointer, length), x, y),
    };
    this.program = new Program(this.host);
    // What a function of the program that trapped said: it stops until the next build.
    this.stopped = null;
    // Whether the next build is of another program, to start over with.
    this.fresh = false;
    // Builds from before this edit are of another program.
    this.first = 0;
    this.last = performance.now();
    this.canvas.addEventListener("click", (event) => this.run("click", event.offsetX, event.offsetY));
    requestAnimationFrame((now) => this.frame(now));
  }

  show(name, text) {
    this.program = new Program(this.host);
    this.stopped = null;
    this.fresh = true;
    this.text = text;
    this.edited += 1;
    this.first = this.edited;
    this.line("info", name, [this.ready ? "building…" : "loading the compiler…"]);
    this.state("building");
    this.build();
  }

  options() {
    const fresh = this.fresh;
    this.fresh = false;
    return { fresh };
  }

  describe({ wasm_bytes, core_bytes, fetch_ms, start_ms }) {
    return [`compiler ${mib(wasm_bytes)}`, `core source ${mib(core_bytes)}`, `fetched in ${ms(fetch_ms)}`, `started in ${ms(start_ms)}`];
  }

  apply({ id, report, module }) {
    if (id < this.first) {
      return null;
    }
    this.program.apply(module, report);
    this.stopped = null;
    return {
      parts: [`${report.bodies} in the program`, `analysis ${ms(report.listing_ms)}`],
      notes: errors(report),
    };
  }

  run(name, ...args) {
    if (shown !== this || this.stopped !== null || !(name in this.program.exports)) {
      return;
    }
    try {
      this.program.call(name, ...args);
    } catch (error) {
      this.stopped = error;
      this.fail(`\`${name}\` stopped`, error);
    }
  }

  frame(now) {
    const scale = devicePixelRatio;
    const { clientWidth: width, clientHeight: height } = this.canvas;
    if (this.canvas.width !== width * scale || this.canvas.height !== height * scale) {
      this.canvas.width = width * scale;
      this.canvas.height = height * scale;
    }
    this.context.setTransform(scale, 0, 0, scale, 0, 0);
    this.context.font = "14px ui-monospace, monospace";
    this.run("frame", width, height, Math.min((now - this.last) / 1000, 0.05));
    this.last = now;
    requestAnimationFrame((now) => this.frame(now));
  }
}

// What the program may fill of the module it is linked into: slots at the end of its table,
// and memory its allocator has not been given.
const SLOTS = 1 << 17;
const BYTES = 64 << 20;

// The Dioxus app. Its Dioxus and `std` are the page's own, the module rustc built. The
// compiler builds the app from the packed workspace, and each build is instantiated in that
// module's memory and table: the page then renders the app's `App` as a component of its.
class Dioxus extends Builds {
  constructor() {
    super("worker.js?pack=pkg/dioxus.pack", 200);
    this.launched = false;
    this.program = new Program({}, page);
    this.start = {
      table: this.program.table.grow(SLOTS),
      memory: this.program.memory.grow(BYTES >> 16) << 16,
    };
  }

  show(name, text) {
    if (text !== this.text) {
      this.line("info", name, [this.ready ? "building…" : "loading Dioxus and the compiler, which takes a while…"]);
      this.state("building");
      this.edit(text);
    } else {
      this.state(this.kind);
    }
  }

  options() {
    const start = this.placed ? undefined : this.start;
    this.placed = true;
    return { start };
  }

  describe({ wasm_bytes, core_bytes, fetch_ms, start_ms, analysis }) {
    return [`compiler ${mib(wasm_bytes)}`, `workspace ${mib(core_bytes)}`, `fetched in ${ms(fetch_ms)}`, `loaded in ${ms(start_ms)}`, analysis];
  }

  apply({ report, module }) {
    const { start, program } = this;
    if (report.table_len > start.table + SLOTS || report.memory_len > start.memory + BYTES) {
      throw new Error("the program outgrew what the page set aside for it");
    }
    program.apply(module, report);
    if (!this.launched) {
      const app = Object.entries(report.exports).find(([name]) => name.endsWith("::App"));
      if (!app) {
        throw new Error("the program has no `App`");
      }
      program.base.blaze_launch(app[1]);
      this.launched = true;
    } else {
      program.base.blaze_rerender();
    }
    return { parts: [], notes: errors(report) };
  }
}

// The list of builds stays at its newest line, unless it was scrolled away from it. The
// app puts the list in the page once, some time after this runs.
new MutationObserver((_, found) => {
  const list = document.getElementById("status");
  if (list === null) {
    return;
  }
  found.disconnect();
  let newest = true;
  list.addEventListener("scroll", () => {
    newest = list.scrollHeight - list.scrollTop - list.clientHeight < 8;
  });
  new MutationObserver(() => newest && (list.scrollTop = list.scrollHeight)).observe(list, { childList: true });
}).observe(document.getElementById("playground"), { childList: true, subtree: true });

// The module keeps every import wasm-bindgen can ask for, among them the promise integration
// some browsers lack. Nothing here calls through it, so the plain function stands in.
WebAssembly.Suspending ??= function (inner) {
  return inner;
};
// The page's own module, which a Dioxus program is linked into.
const page = await init();

const runners = {};
window.blaze = {
  // Each compiler is loaded when a program of its kind is first shown, and kept.
  show(name, text) {
    const kind = name === "dioxus" ? Dioxus : Canvas;
    shown = runners[kind.name] ??= new kind();
    shown.show(name, text);
  },
  edit: (text) => shown?.edit(text),
};

// Tab is four spaces in the editor.
document.addEventListener("keydown", (event) => {
  if (event.key === "Tab" && event.target instanceof HTMLTextAreaElement) {
    event.preventDefault();
    const area = event.target;
    area.setRangeText("    ", area.selectionStart, area.selectionEnd, "end");
    area.dispatchEvent(new InputEvent("input", { bubbles: true }));
  }
});

page.blaze_main();
