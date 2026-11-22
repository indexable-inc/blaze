/**
 * Live telemetry stream protocol between a local `blaze-launcher --viewer` (127.0.0.1:41937)
 * and `https://ix.dev/blaze/explore` (or `http://127.0.0.1:41937/explore`).
 *
 * Transport negotiation:
 *   1. Probes `/api/info` on loopback. If the local viewer advertises a WebTransport HTTP/3
 *      endpoint and a SHA-256 `serverCertificateHashes` digest (and the browser exposes
 *      `WebTransport`), upgrades to bidirectional WebTransport streams.
 *   2. Otherwise connects via `ws://127.0.0.1:41937/ws` (RFC 6455 WebSocket, exempt from
 *      mixed-content blocking for 127.0.0.1 in Chrome/Firefox/Safari) with automatic
 *      reconnect so one open tab stays live across every `blaze run`.
 */

export const DEFAULT_VIEWER_ORIGIN = 'http://127.0.0.1:41937';

export type EventKindName =
	| 'span_enter'
	| 'span_exit'
	| 'span_unwound'
	| 'hook_hit'
	| 'alloc'
	| 'free'
	| 'io'
	| 'sync_wait'
	| 'thread_birth'
	| 'thread_end'
	| 'sample';

export type HookKindName = 'plain' | 'count' | 'mark' | 'hook' | 'span';

export interface RunStats {
	cap: number;
	pushed: number;
	popped: number;
	dropped: number;
	shadow_overflows: number;
	unwound_spans: number;
}

export interface RunSummary {
	id: string;
	pid: number;
	package: string;
	alive: boolean;
	started_ms: number;
	stats: RunStats;
	slots: Record<string, string>;
}

export interface TelemetryEvent {
	run: string;
	kind: EventKindName;
	flags: number;
	cpu: number;
	thread: number;
	slot_or_op: number;
	ts_ns: number;
	a: number;
	b: number;
	c: number;
	d: number;
	e: number;
}

export interface HostTopology {
	arch: string;
	os: string;
	brand: string;
	ncpu: number;
	p_cores: number;
	e_cores: number;
	numa_nodes: number;
	l1d_bytes: number;
	l2_bytes: number;
	mem_bytes: number;
}

export interface SourceSnippet {
	file: string;
	line: number;
	lines: Array<{ n: number; text: string }>;
}

export interface SlotLocation {
	name: string;
	file?: string;
	line?: number;
	rebuilt?: boolean;
}

export interface ViewerInfo {
	version: number;
	active_viewers: number;
	webtransport_url?: string;
	webtransport_cert_sha256_b64?: string;
}

export interface BuildPhase {
	name: string;
	ns: number;
}

export interface BuildSummary {
	id: string;
	package: string;
	command: string;
	status: 'building' | 'running' | 'ok' | 'failed';
	exit_code: number | null;
	errors: number;
	warnings: number;
	reached: number;
	found: number;
	made: number;
	placed: number;
	resolved: number;
	frontend_built: string;
	recompile_reason: string;
	phases: BuildPhase[];
	slots?: Record<string, string>;
	locations?: Record<string, SlotLocation>;
}

export type ServerMessage =
	| { type: 'runs'; runs: RunSummary[] }
	| { type: 'builds'; builds: BuildSummary[] }
	| { type: 'run_started'; run: RunSummary }
	| { type: 'run_ended'; id: string; status: number }
	| { type: 'events'; events: TelemetryEvent[] };

export type TransportKind = 'webtransport' | 'websocket' | 'disconnected';

interface WebTransportLike {
	ready: Promise<void>;
	closed: Promise<unknown>;
	incomingUnidirectionalStreams: ReadableStream<ReadableStream<Uint8Array>>;
	createBidirectionalStream(): Promise<{
		writable: WritableStream<Uint8Array>;
		readable: ReadableStream<Uint8Array>;
	}>;
	close(): void;
}

type WebTransportCtor = new (
	url: string,
	options?: { serverCertificateHashes?: Array<{ algorithm: string; value: ArrayBuffer }> },
) => WebTransportLike;

/** Only loopback viewer origins are permitted (prevents arbitrary origin redirection). */
export function resolveViewerOrigin(search: string): string {
	const params = new URLSearchParams(search);
	const raw = params.get('viewer');
	if (!raw) return DEFAULT_VIEWER_ORIGIN;
	try {
		const parsed = new URL(raw);
		if (
			(parsed.protocol === 'http:' || parsed.protocol === 'https:') &&
			(parsed.hostname === '127.0.0.1' || parsed.hostname === 'localhost')
		) {
			return parsed.origin;
		}
	} catch {
		// Fall through to default loopback origin.
	}
	return DEFAULT_VIEWER_ORIGIN;
}

function base64ToBytes(b64: string): ArrayBuffer {
	const bin = atob(b64);
	const out = new Uint8Array(bin.length);
	for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
	return out.buffer;
}

export interface LiveHandlers {
	onStatus(transport: TransportKind, detail: string): void;
	onMessage(msg: ServerMessage): void;
	onBinaryEvents?(rawPayload: Uint8Array, runId: string): void;
}

export class LiveClient {
	private ws: WebSocket | null = null;
	private wt: WebTransportLike | null = null;
	private timer: number | null = null;
	private closed = false;
	private backoffMs = 250;

	constructor(
		private readonly origin: string,
		private readonly handlers: LiveHandlers,
	) {}

	start(): void {
		this.closed = false;
		void this.connect();
	}

	stop(): void {
		this.closed = true;
		if (this.timer !== null) {
			window.clearTimeout(this.timer);
			this.timer = null;
		}
		this.ws?.close();
		this.ws = null;
		this.wt?.close();
		this.wt = null;
	}

	async fetchTopology(): Promise<HostTopology | null> {
		const origins =
			window.location.origin && window.location.origin !== this.origin
				? [this.origin, window.location.origin]
				: [this.origin];
		for (const base of origins) {
			try {
				const res = await fetch(`${base}/api/topology`, { cache: 'no-store' });
				if (res.ok) return (await res.json()) as HostTopology;
			} catch {
				// Try next origin.
			}
		}
		return null;
	}

	async fetchSource(file: string, line: number): Promise<SourceSnippet | null> {
		const origins =
			window.location.origin && window.location.origin !== this.origin
				? [this.origin, window.location.origin]
				: [this.origin];
		const qs = `/api/source?file=${encodeURIComponent(file)}&line=${line}`;
		for (const base of origins) {
			try {
				const res = await fetch(`${base}${qs}`, { cache: 'no-store' });
				if (res.ok) return (await res.json()) as SourceSnippet;
			} catch {
				// Try next origin.
			}
		}
		return null;
	}

	async toggleHook(runId: string, slot: number, kind: HookKindName): Promise<void> {
		const payload = JSON.stringify({ type: 'toggle', run: runId, slot, kind });
		if (this.ws && this.ws.readyState === WebSocket.OPEN) {
			this.ws.send(payload);
			return;
		}
		await fetch(`${this.origin}/api/toggle`, {
			method: 'POST',
			headers: { 'content-type': 'application/json' },
			body: payload,
		});
	}

	private scheduleReconnect(): void {
		if (this.closed) return;
		this.handlers.onStatus('disconnected', `Waiting for local blaze run on ${this.origin}...`);
		const wait = this.backoffMs;
		this.backoffMs = Math.min(this.backoffMs * 1.5, 2500);
		this.timer = window.setTimeout(() => {
			this.timer = null;
			void this.connect();
		}, wait);
	}

	private async connect(): Promise<void> {
		if (this.closed) return;
		let info: ViewerInfo | null = null;
		try {
			const res = await fetch(`${this.origin}/api/info`, { cache: 'no-store' });
			if (res.ok) info = (await res.json()) as ViewerInfo;
		} catch {
			// Local viewer not up yet; retry shortly.
			this.scheduleReconnect();
			return;
		}

		// Prefer WebTransport when the local viewer advertises an H3 endpoint + cert hash.
		const wtCtor = (globalThis as unknown as { WebTransport?: WebTransportCtor }).WebTransport;
		if (wtCtor && info?.webtransport_url && info.webtransport_cert_sha256_b64) {
			try {
				const wt = new wtCtor(info.webtransport_url, {
					serverCertificateHashes: [
						{ algorithm: 'sha-256', value: base64ToBytes(info.webtransport_cert_sha256_b64) },
					],
				});
				await wt.ready;
				this.wt = wt;
				this.backoffMs = 250;
				this.handlers.onStatus('webtransport', `WebTransport · ${info.webtransport_url}`);
				void this.pumpWebTransport(wt);
				return;
			} catch {
				// Fall through to WebSocket on the same loopback viewer.
			}
		}

		this.connectWebSocket();
	}

	private async pumpWebTransport(wt: WebTransportLike): Promise<void> {
		const decoder = new TextDecoder();
		try {
			const reader = wt.incomingUnidirectionalStreams.getReader();
			while (!this.closed) {
				const { value: stream, done } = await reader.read();
				if (done || !stream) break;
				const sr = stream.getReader();
				let text = '';
				while (true) {
					const chunk = await sr.read();
					if (chunk.done) break;
					text += decoder.decode(chunk.value, { stream: true });
				}
				text += decoder.decode();
				for (const line of text.split('\n')) {
					if (!line) continue;
					try {
						this.handlers.onMessage(JSON.parse(line) as ServerMessage);
					} catch {
						// Ignore malformed frame.
					}
				}
			}
		} catch {
			// Reconnect below.
		}
		this.wt = null;
		this.scheduleReconnect();
	}

	private connectWebSocket(): void {
		const wsUrl = `${this.origin.replace(/^http/, 'ws')}/ws`;
		const ws = new WebSocket(wsUrl);
		ws.binaryType = 'arraybuffer';
		this.ws = ws;
		ws.addEventListener('open', () => {
			this.backoffMs = 250;
			this.handlers.onStatus('websocket', `Live · ${wsUrl} (binary)`);
		});
		ws.addEventListener('message', (ev) => {
			if (ev.data instanceof ArrayBuffer) {
				const decoded = decodeBinaryEventsWithRaw(ev.data);
				if (decoded) {
					this.handlers.onBinaryEvents?.(decoded.raw, decoded.run);
					this.handlers.onMessage(decoded.msg);
				}
				return;
			}
			if (typeof ev.data !== 'string') return;
			try {
				this.handlers.onMessage(JSON.parse(ev.data) as ServerMessage);
			} catch {
				// Ignore malformed frame.
			}
		});
		ws.addEventListener('close', () => {
			if (this.ws === ws) this.ws = null;
			this.scheduleReconnect();
		});
		ws.addEventListener('error', () => {
			ws.close();
		});
	}
}

const EVENT_KIND_TABLE: Record<number, EventKindName> = {
	1: 'span_enter',
	2: 'span_exit',
	3: 'span_unwound',
	4: 'hook_hit',
	5: 'alloc',
	6: 'free',
	7: 'io',
	8: 'sync_wait',
	9: 'thread_birth',
	10: 'thread_end',
	11: 'sample',
};

const utf8Decoder = new TextDecoder();

/**
 * Decodes a packed binary WebSocket event batch from `blaze-launcher --viewer` (zero JSON.parse):
 *   [u8 magic='B', u8 ver=1, u16 run_id_len (LE), run_id bytes..., pad to 8B, N * 64B Event]
 */
export function decodeBinaryEvents(buf: ArrayBuffer): ServerMessage | null {
	return decodeBinaryEventsWithRaw(buf)?.msg ?? null;
}

export function decodeBinaryEventsWithRaw(
	buf: ArrayBuffer,
): { run: string; raw: Uint8Array; msg: ServerMessage } | null {
	if (buf.byteLength < 8) return null;
	const view = new DataView(buf);
	if (view.getUint8(0) !== 0x42 || view.getUint8(1) !== 1) return null;
	const idLen = view.getUint16(2, true);
	const hdrLen = (4 + idLen + 7) & ~7;
	if (hdrLen > buf.byteLength) return null;
	const run = utf8Decoder.decode(new Uint8Array(buf, 4, idLen));
	const count = Math.floor((buf.byteLength - hdrLen) / 64);
	const raw = new Uint8Array(buf, hdrLen, count * 64);
	const events: TelemetryEvent[] = new Array(count);
	for (let i = 0; i < count; i++) {
		const off = hdrLen + i * 64;
		events[i] = {
			run,
			kind: EVENT_KIND_TABLE[view.getUint8(off)] ?? 'sample',
			flags: view.getUint8(off + 1),
			cpu: view.getUint16(off + 2, true),
			thread: view.getUint32(off + 4, true),
			slot_or_op: view.getUint32(off + 8, true),
			ts_ns: Number(view.getBigUint64(off + 16, true)),
			a: Number(view.getBigUint64(off + 24, true)),
			b: Number(view.getBigUint64(off + 32, true)),
			c: Number(view.getBigUint64(off + 40, true)),
			d: Number(view.getBigUint64(off + 48, true)),
			e: Number(view.getBigUint64(off + 56, true)),
		};
	}
	return { run, raw, msg: { type: 'events', events } };
}
