// The program's functions and calls as the page holds them: the snapshot, then every Functions
// record merged in by key (docs/web.md, "Where it comes from").
import { moduleOf } from "./names.js";
import { HOW_COMPILED, HOW_UNIT, HOW_UNCHANGED, HOW_STORED } from "./snapshot.js";

const HOW_OF = { Compiled: HOW_COMPILED, Unit: HOW_UNIT, Unchanged: HOW_UNCHANGED, Stored: HOW_STORED };

function grow(a, n) {
  if (n <= a.length) return a;
  const b = new a.constructor(Math.max(n, a.length * 2));
  b.set(a);
  return b;
}

export class Model {
  constructor(snap) {
    this.root = snap.root;
    this.build = snap.build;
    // The build the page shows: what it did not compile is drawn as cached.
    this.current = snap.build;
    this.crates = snap.crates.slice();
    this.crateIndex = new Map(this.crates.map((c, i) => [c, i]));
    this.n = snap.n;
    this.key = snap.key;
    this.krate = snap.krate;
    this.how = snap.how;
    this.ns = snap.ns;
    this.nodeBuild = snap.nodeBuild;
    this.names = snap.names;
    this.byKey = snap.byKey;
    this.group = new Uint32Array(this.n);
    this.modules = [];
    this.moduleCrate = [];
    this.moduleIndex = new Map();
    for (let i = 0; i < this.n; i++) this.group[i] = this.moduleFor(this.names[i], this.krate[i]);
    this.m = snap.edges.length / 2;
    this.edges = snap.edges;
    this.out = Array.from({ length: this.n }, () => []);
    this.inn = Array.from({ length: this.n }, () => []);
    for (let e = 0; e < this.m; e++) {
      this.out[this.edges[2 * e]].push(e);
      this.inn[this.edges[2 * e + 1]].push(e);
    }
    // Per object: id -> node, and the calls that name an id no record of it has said yet.
    this.objects = new Map();
    this.unresolvedCalls = 0;
  }

  moduleFor(name, krate) {
    const crateName = this.crates[krate];
    const mod = moduleOf(name, crateName);
    const k = krate + "\u0000" + mod;
    let g = this.moduleIndex.get(k);
    if (g === undefined) {
      g = this.modules.length;
      this.moduleIndex.set(k, g);
      this.modules.push(mod);
      this.moduleCrate.push(krate);
    }
    return g;
  }

  crateFor(name) {
    let c = this.crateIndex.get(name);
    if (c === undefined) {
      c = this.crates.length;
      this.crates.push(name);
      this.crateIndex.set(name, c);
    }
    return c;
  }

  hasEdge(a, b) {
    for (const e of this.out[a]) if (this.edges[2 * e + 1] === b) return true;
    return false;
  }

  addEdge(a, b) {
    if (a === b || this.hasEdge(a, b)) return false;
    this.edges = grow(this.edges, 2 * (this.m + 1));
    this.edges[2 * this.m] = a;
    this.edges[2 * this.m + 1] = b;
    this.out[a].push(this.m);
    this.inn[b].push(this.m);
    this.m++;
    return true;
  }

  addNode(key, krate, name) {
    const i = this.n++;
    this.key = grow(this.key, this.n);
    this.krate = grow(this.krate, this.n);
    this.how = grow(this.how, this.n);
    this.ns = grow(this.ns, this.n);
    this.nodeBuild = grow(this.nodeBuild, this.n);
    this.group = grow(this.group, this.n);
    this.key[i] = key;
    this.krate[i] = krate;
    this.names[i] = name;
    this.group[i] = this.moduleFor(name, krate);
    this.out.push([]);
    this.inn.push([]);
    this.byKey.set(key, i);
    return i;
  }

  // Merges one Functions record. Returns the nodes it names, the ones it added and whether calls were added.
  mergeFns(object, fns, build) {
    let obj = this.objects.get(object);
    if (!obj) {
      obj = { ids: new Map(), pending: new Map() };
      this.objects.set(object, obj);
    }
    const touched = [];
    const added = [];
    let newEdges = 0;
    for (const f of fns) {
      const key = BigInt(f.key);
      let i = this.byKey.get(key);
      const krate = this.crateFor(f.krate);
      if (i === undefined) {
        i = this.addNode(key, krate, f.name);
        added.push(i);
      } else if (this.names[i] !== f.name || this.krate[i] !== krate) {
        this.names[i] = f.name;
        this.krate[i] = krate;
        this.group[i] = this.moduleFor(f.name, krate);
      }
      this.how[i] = HOW_OF[f.how.type];
      this.ns[i] = f.how.type === "Compiled" ? f.how.ns : 0;
      this.nodeBuild[i] = build;
      if (obj.ids.has(f.id) && obj.ids.get(f.id) !== i) {
        throw new Error(`Functions of object ${object}: id ${f.id} is said for two functions`);
      }
      obj.ids.set(f.id, i);
      touched.push(i);
      const waiting = obj.pending.get(f.id);
      if (waiting) {
        for (const from of waiting) if (this.addEdge(from, i)) newEdges++;
        this.unresolvedCalls -= waiting.length;
        obj.pending.delete(f.id);
      }
    }
    for (let k = 0; k < fns.length; k++) {
      const from = touched[k];
      for (const id of fns[k].calls) {
        const to = obj.ids.get(id);
        if (to !== undefined) {
          if (this.addEdge(from, to)) newEdges++;
        } else {
          let w = obj.pending.get(id);
          if (!w) obj.pending.set(id, (w = []));
          w.push(from);
          this.unresolvedCalls++;
        }
      }
    }
    return { touched, added, newEdges };
  }

  counts() {
    const c = [0, 0, 0, 0];
    for (let i = 0; i < this.n; i++) c[this.how[i]]++;
    return { compiled: c[HOW_COMPILED], unit: c[HOW_UNIT], unchanged: c[HOW_UNCHANGED], stored: c[HOW_STORED] };
  }

  // Nodes within `hops` calls of `start`, by direction: [{node, depth, edge}], nearest first,
  // `edge` the call it was reached by.
  neighbourhood(start, hops, dir) {
    const seen = new Map([[start, 0]]);
    let frontier = [start];
    const out = [];
    for (let d = 1; d <= hops && frontier.length; d++) {
      const next = [];
      for (const x of frontier) {
        const list = dir === "callees" ? this.out[x] : this.inn[x];
        for (const e of list) {
          const y = this.edges[2 * e + (dir === "callees" ? 1 : 0)];
          if (!seen.has(y)) {
            seen.set(y, d);
            next.push(y);
            out.push({ node: y, depth: d, edge: e });
          }
        }
      }
      frontier = next;
    }
    return out;
  }

  // Each crate's and module's function count and compile time, and crate -> crate calls.
  aggregates() {
    const nc = this.crates.length;
    const crateCount = new Float64Array(nc);
    const crateNs = new Float64Array(nc);
    const moduleCount = new Float64Array(this.modules.length);
    const moduleNs = new Float64Array(this.modules.length);
    for (let i = 0; i < this.n; i++) {
      crateCount[this.krate[i]]++;
      crateNs[this.krate[i]] += this.ns[i];
      moduleCount[this.group[i]]++;
      moduleNs[this.group[i]] += this.ns[i];
    }
    const crateEdges = new Map();
    for (let e = 0; e < this.m; e++) {
      const a = this.krate[this.edges[2 * e]], b = this.krate[this.edges[2 * e + 1]];
      if (a === b) continue;
      const k = a * nc + b;
      crateEdges.set(k, (crateEdges.get(k) ?? 0) + 1);
    }
    return { crateCount, crateNs, moduleCount, moduleNs, crateEdges };
  }
}
