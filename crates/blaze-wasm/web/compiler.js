// The compiler: `blaze_wasm.wasm` with its exports as methods.

export class Compiler {
  // `wasm` is the compiler's module, `core` the bundle of `core`'s source.
  static async load(wasm, core) {
    if (!(wasm instanceof WebAssembly.Module)) {
      wasm = await WebAssembly.compile(wasm);
    }
    let memory;
    const text = (pointer, length) =>
      new TextDecoder().decode(new Uint8Array(memory.buffer, pointer >>> 0, length >>> 0).slice());
    // What the compiler needs of the page: a clock, and somewhere to say why it panicked.
    const blaze = {
      now: () => performance.now(),
      log: (pointer, length) => console.error(text(pointer, length)),
    };
    const instance = await WebAssembly.instantiate(wasm, { blaze });
    memory = instance.exports.memory;
    const compiler = new Compiler(instance.exports);
    if (core && !compiler.blaze.blaze_add_bundle(...compiler.argument(core))) {
      throw new Error(compiler.report());
    }
    return compiler;
  }

  constructor(blaze) {
    this.blaze = blaze;
  }

  // Copies bytes or text into the compiler, which frees them once it has read them.
  argument(value) {
    const bytes = typeof value === "string" ? new TextEncoder().encode(value) : value;
    const pointer = this.blaze.blaze_alloc(bytes.length);
    new Uint8Array(this.blaze.memory.buffer, pointer >>> 0, bytes.length).set(bytes);
    return [pointer, bytes.length];
  }

  // Sets a packed cargo workspace as what there is to compile.
  setPack(pack) {
    if (!this.blaze.blaze_set_pack(...this.argument(pack))) {
      throw new Error(this.report());
    }
  }

  // Where in the memory and table of the module the program lives in it may put its
  // functions and data.
  setStart(tableStart, memoryStart) {
    if (!this.blaze.blaze_set_start(tableStart, memoryStart)) {
      throw new Error(this.report());
    }
  }

  analyze() {
    this.blaze.blaze_analyze();
    return this.report();
  }

  setFile(path, text) {
    this.blaze.blaze_set_file(...this.argument(path), ...this.argument(text));
  }

  report() {
    const { memory, blaze_report, blaze_report_len } = this.blaze;
    const bytes = new Uint8Array(memory.buffer, blaze_report() >>> 0, blaze_report_len());
    return new TextDecoder().decode(bytes.slice());
  }

  // Builds what changed. Gives the module to instantiate and the build's report, or throws
  // with what the compiler said.
  build() {
    if (!this.blaze.blaze_build()) {
      throw new Error(this.report());
    }
    const { memory, blaze_module, blaze_module_len } = this.blaze;
    const module = new Uint8Array(memory.buffer, blaze_module() >>> 0, blaze_module_len()).slice();
    return { module, report: JSON.parse(this.report()) };
  }

  // How much memory the compiler has.
  memoryBytes() {
    return this.blaze.memory.buffer.byteLength;
  }
}
