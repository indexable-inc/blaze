// The program the compiler builds, as it runs in a page: one memory, one function table and
// one stack pointer, and every build's module instantiated over them.

export class Program {
  // `host` has the functions the program declares in `extern` blocks, which a module imports
  // from `env` as rustc's would. `base` is the exports of a module whose memory, table and
  // stack pointer the program lives in, when there is one.
  constructor(host, base = null) {
    this.base = base;
    this.memory = base?.memory ?? new WebAssembly.Memory({ initial: 32 });
    this.table =
      base?.__indirect_function_table ?? new WebAssembly.Table({ initial: 1, element: "anyfunc" });
    this.stackPointer =
      base?.__stack_pointer ?? new WebAssembly.Global({ value: "i32", mutable: true }, 0);
    this.exports = {};
    // A function nobody provides fails when it is called, not when the module is linked.
    // What a Rust program expects of its C library, where the compiler does not inline it.
    const compare = (left, right, length) => {
      const [a, b] = [this.bytes(left, length), this.bytes(right, length)];
      const at = a.findIndex((byte, index) => byte !== b[index]);
      return at < 0 ? 0 : a[at] - b[at];
    };
    const own = { memory: this.memory, table: this.table, stack_pointer: this.stackPointer };
    host = { memcmp: compare, bcmp: compare, ...host, ...own };
    this.env = new Proxy(host, {
      get: (host, name) =>
        host[name] ??
        (() => {
          throw new Error(`the program calls \`${String(name)}\`, which the page does not provide`);
        }),
      has: () => true,
    });
    // A function of an earlier module that this one calls directly, by its slot.
    this.slots = new Proxy({}, { get: (_, slot) => this.table.get(Number(slot)), has: () => true });
  }

  // Instantiates the module of one build. `report` is the build's JSON, parsed.
  apply(module, report) {
    if (this.table.length < report.table_len) {
      this.table.grow(report.table_len - this.table.length);
    }
    const pages = Math.ceil(report.memory_len / 65536);
    const have = this.memory.buffer.byteLength / 65536;
    if (have < pages) {
      this.memory.grow(pages - have);
    }
    if (this.stackPointer.value === 0) {
      this.stackPointer.value = report.stack_top;
    }
    new WebAssembly.Instance(new WebAssembly.Module(module), { env: this.env, slot: this.slots });
    Object.assign(this.exports, report.exports);
  }

  // Calls a function of the program by symbol, through its slot.
  call(name, ...args) {
    const slot = this.exports[name];
    if (slot === undefined) {
      throw new Error(`the program has no function \`${name}\``);
    }
    return this.table.get(slot)(...args);
  }

  // The bytes at a pointer the program handed over.
  bytes(pointer, length) {
    return new Uint8Array(this.memory.buffer, pointer >>> 0, length >>> 0);
  }

  text(pointer, length) {
    return new TextDecoder().decode(this.bytes(pointer, length));
  }
}
