// What wrangler bundles from .gen/ (gen/build.ts writes both): the renderer as a compiled module (wasm files are
// CompiledWasm by default) and the site's snapshot as bytes (the `Data` rule for *.cbor in wrangler.jsonc).
declare module '*.wasm' {
	const module: WebAssembly.Module;
	export default module;
}
declare module '*.cbor' {
	const data: ArrayBuffer;
	export default data;
}
