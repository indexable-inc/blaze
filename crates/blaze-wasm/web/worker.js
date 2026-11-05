// The compiler runs here, off the page's thread: the first build takes seconds.

// Under the page's query, which names the build, so that it is the compiler's own JavaScript.
const { Compiler } = await import(`./compiler.js${location.search}`);

// `?pack=<url>` names a packed cargo workspace to compile instead of one file over `core`.
// `?gz` says the files are the gzipped ones `build-site.sh` makes, for a host that does not
// compress what it serves.
const query = new URL(location.href).searchParams;
const pack = query.get("pack");
const suffix = query.has("gz") ? ".gz" : "";
let compiler;
let compiled;
let core;

// The bytes at `url`, saying how many have arrived as they do.
const received = {};
async function load(url) {
  // Asked of the server each time: a compiler the browser kept from an earlier build does
  // not have what this JavaScript calls.
  const response = await fetch(url, { cache: "no-cache" });
  if (!response.ok) {
    throw new Error(`${url}: ${response.status}`);
  }
  const chunks = [];
  received[url] = 0;
  for (const reader = response.body.getReader(); ; ) {
    const { done, value } = await reader.read();
    if (done) {
      break;
    }
    chunks.push(value);
    received[url] += value.length;
    postMessage({ progress: Object.values(received).reduce((sum, it) => sum + it, 0) });
  }
  let bytes = new Blob(chunks);
  // A server that knows gzip may have undone it already.
  const magic = new Uint8Array(await bytes.slice(0, 2).arrayBuffer());
  if (suffix && magic[0] === 0x1f && magic[1] === 0x8b) {
    if (typeof DecompressionStream === "undefined") {
      throw new Error("this browser has no DecompressionStream");
    }
    bytes = new Response(bytes.stream().pipeThrough(new DecompressionStream("gzip")));
  }
  return new Uint8Array(await bytes.arrayBuffer());
}

try {
  const started = performance.now();
  let wasm;
  [wasm, core] = await Promise.all(
    ["pkg/blaze_wasm.wasm", pack ?? "pkg/core.bundle"].map((url) => load(url + suffix)),
  );
  const fetched = performance.now();
  compiled = await WebAssembly.compile(wasm);
  compiler = await Compiler.load(compiled, pack ? null : core);
  let analysis;
  if (pack) {
    compiler.setPack(core);
    analysis = compiler.analyze();
  }
  postMessage({
    loaded: {
      wasm_bytes: wasm.length,
      core_bytes: core.length,
      fetch_ms: fetched - started,
      start_ms: performance.now() - fetched,
      analysis,
    },
  });
} catch (error) {
  // A worker's own error reaches the page without its message.
  postMessage({ error: `the compiler failed to load: ${error.message ?? error}` });
  throw error;
}

// A `fresh` build is of another program: nothing of the one before is kept.
// `start` is where the program may put its functions and data in a module it is linked into.
onmessage = async ({ data: { id, text, fresh, start } }) => {
  const began = performance.now();
  try {
    if (fresh && !pack) {
      compiler = await Compiler.load(compiled, core);
    }
    if (start) {
      compiler.setStart(start.table, start.memory);
    }
    compiler.setFile("/app/main.rs", text);
    const { module, report } = compiler.build();
    report.build_ms = performance.now() - began;
    report.compiler_bytes = compiler.memoryBytes();
    postMessage({ id, module, report }, [module.buffer]);
  } catch (error) {
    postMessage({ id, error: String(error.message ?? error) });
  }
};
