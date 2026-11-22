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
	thread: number;
	slot_or_op: number;
	ts_ns: number;
	a: number;
	b: number;
	c: number;
	d: number;
	e: number;
}

export interface ViewerInfo {
	version: number;
	active_viewers: number;
	webtransport_url?: string;
	webtransport_cert_sha256_b64?: string;
}

export type ServerMessage =
	| { type: 'runs'; runs: RunSummary[] }
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
		this.ws = ws;
		ws.addEventListener('open', () => {
			this.backoffMs = 250;
			this.handlers.onStatus('websocket', `Live · ${wsUrl}`);
		});
		ws.addEventListener('message', (ev) => {
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
