/**
 * Thin zero-copy TypeScript bridge to `explore_core.wasm` (compiled from `site/explore/wasm/`,
 * directly sharing `crates/blaze-launch/src/telemetry.rs`).
 *
 * Raw 64-byte `Event` payloads from binary WebSocket frames copy straight into WASM linear
 * memory (`ingest_buf_ptr`), where Rust tracks per-CPU core utilization, thread migrations,
 * and builds NDC WebGPU instance buffers directly in linear memory.
 */

interface ExploreWasmExports {
	memory: WebAssembly.Memory;
	ingest_buf_ptr(): number;
	ingest_buf_cap(): number;
	gpu_rects_ptr(): number;
	cpu_events_ptr(): number;
	cpu_busy_us_ptr(): number;
	total_migrations(): number;
	sample_count(): number;
	clear_state(): void;
	ingest_events(byteLen: number, nowMs: number): number;
	build_gpu_rects(
		viewStartMs: number,
		viewEndMs: number,
		cssW: number,
		cssH: number,
		scrollY: number,
		gutterW: number,
	): number;
}

export class ExploreWasmCore {
	private exports: ExploreWasmExports | null = null;

	get ready(): boolean {
		return this.exports !== null;
	}

	async load(url = 'explore/explore_core.wasm'): Promise<boolean> {
		try {
			const res = await fetch(url);
			if (!res.ok) return false;
			const bytes = await res.arrayBuffer();
			const { instance } = await WebAssembly.instantiate(bytes, {});
			this.exports = instance.exports as unknown as ExploreWasmExports;
			return true;
		} catch {
			return false;
		}
	}

	/** Copies raw 64-byte Event slice directly into WASM linear memory and ingests in Rust. */
	ingestRaw(payload: Uint8Array, nowMs: number): number {
		const ex = this.exports;
		if (!ex || payload.byteLength < 64) return 0;
		const cap = ex.ingest_buf_cap();
		const len = Math.min(payload.byteLength, cap) & ~63;
		const dst = new Uint8Array(ex.memory.buffer, ex.ingest_buf_ptr(), len);
		dst.set(payload.subarray(0, len));
		return ex.ingest_events(len, nowMs);
	}

	/** Reads live per-CPU event counts & busy microseconds directly from WASM linear memory. */
	cpuStats(ncpu: number): { events: Uint32Array; busyUs: Uint32Array; migrations: number } | null {
		const ex = this.exports;
		if (!ex) return null;
		const n = Math.min(128, Math.max(1, ncpu));
		return {
			events: new Uint32Array(ex.memory.buffer, ex.cpu_events_ptr(), n),
			busyUs: new Uint32Array(ex.memory.buffer, ex.cpu_busy_us_ptr(), n),
			migrations: ex.total_migrations(),
		};
	}

	/** Zero-copy view of WebGPU NDC rects computed by Rust in WASM linear memory. */
	buildGpuRects(
		viewStartMs: number,
		viewEndMs: number,
		cssW: number,
		cssH: number,
		scrollY: number,
		gutterW: number,
	): Float32Array | null {
		const ex = this.exports;
		if (!ex) return null;
		const count = ex.build_gpu_rects(viewStartMs, viewEndMs, cssW, cssH, scrollY, gutterW);
		return new Float32Array(ex.memory.buffer, ex.gpu_rects_ptr(), count * 8);
	}

	clear(): void {
		this.exports?.clear_state();
	}
}
