// The blob store (API.md): /has, /get, /blob/<sha256>, /put over packs in R2 and locations in D1.
//
// The namespaces a request may read are fixed from its token before any lookup, and a private
// namespace is looked up only by HMAC keys, so another tenant's blob is absent by the same path
// an unknown one takes.

import * as zlib from "node:zlib";
import { lookupTenant, type Caller, type TenantRecord } from "./auth";
import { CborError, decode, encode, type Cbor } from "./cbor";
import { clock, DAY } from "./clock";
import {
  blobRecordHeader,
  buildPack,
  CODEC_RAW,
  CODEC_ZSTD,
  MAX_ULEN,
  missingRecord,
  parseStream,
  STREAM_MAGIC,
  StreamError,
  TAG_END,
  type Frame,
} from "./pack";

export interface CasEnv {
  BUCKET: R2Bucket;
  DB: D1Database;
  TOKENS: KVNamespace;
  [secret: `GATEWAY_SECRET_${number}`]: string | undefined;
}

export const MAX_PUT = 32 * 1024 * 1024;
export const MAX_HASHES = 20_000;
/** Rows per D1 statement: a JSON array bound as one parameter stays well under D1's 2 MB per value. */
const ROWS_PER_STATEMENT = 5_000;
/** Adjacent frames of one object are read together, up to this much per R2 read. */
const MAX_READ = 4 * 1024 * 1024;
/** R2 reads in flight per /get; a Worker keeps at most 6 subrequests waiting for headers. */
const READ_AHEAD = 4;
/** A location whose object was uploaded longer ago than this is absent: the object is about to expire (docs/remote-cache.md, "GC"). */
export const ROW_TTL = 80 * DAY;
/** Rows the background sweep deletes per write, at most. */
const SWEEP_ROWS = 500;

export interface Namespace {
  /** `pub`, or the tenant id. */
  ns: string;
  /** The tenant's HMAC key; null for the public namespace. */
  key: CryptoKey | null;
}

export interface Access {
  /** Own namespace first, then public. */
  read: Namespace[];
  write: Namespace | null;
}

const PUBLIC: Namespace = { ns: "pub", key: null };

export type Answer = { response: Response; bytesIn: number; bytesOut: number; pump?: Promise<void> };

const tenantKeys = new Map<string, { key: CryptoKey; until: number }>();

/** Decided from the token alone (KV, and D1's tenant row for a private tenant), before any blob lookup. */
export async function access(env: CasEnv, caller: Caller): Promise<Access | Response> {
  if (caller.kind !== "token") return { read: [PUBLIC], write: null };
  const tenant = await lookupTenant(env.TOKENS, caller.tenant);
  if (tenant === null) return text(401, "the token's tenant does not exist");
  return accessOf(env, tenant, caller.scope === "write");
}

/** What a tenant's own index may name, and where its writes go. */
export async function accessOf(env: CasEnv, tenant: TenantRecord, write: boolean): Promise<Access> {
  const own = await namespaceOf(env, tenant);
  return { read: tenant.public ? [PUBLIC] : [own, PUBLIC], write: write ? own : null };
}

export async function namespaceOf(env: CasEnv, tenant: TenantRecord): Promise<Namespace> {
  return tenant.public ? PUBLIC : { ns: tenant.id, key: await tenantKey(env, tenant.id) };
}

/** `k_t = HKDF-SHA256(GATEWAY_SECRET_<v>, salt, "blaze tenant key" 0 <id> 0 <v>)`, as an HMAC-SHA256 key. */
async function tenantKey(env: CasEnv, tenantId: string): Promise<CryptoKey> {
  const cached = tenantKeys.get(tenantId);
  if (cached && cached.until > Date.now()) return cached.key;
  const [, rows] = await env.DB.batch<{ salt: ArrayBuffer; key_version: number }>([
    env.DB.prepare("INSERT OR IGNORE INTO tenants (id, salt, key_version) VALUES (?1, randomblob(32), 1)").bind(tenantId),
    env.DB.prepare("SELECT salt, key_version FROM tenants WHERE id = ?1").bind(tenantId),
  ]);
  const row = rows.results[0];
  if (!row) throw new Error(`tenant ${tenantId} has no tenants row after insert`);
  const secret = env[`GATEWAY_SECRET_${row.key_version}`];
  if (typeof secret !== "string" || secret === "") throw new Error(`GATEWAY_SECRET_${row.key_version} is not set`);
  const ikm = await crypto.subtle.importKey("raw", new TextEncoder().encode(secret), "HKDF", false, ["deriveBits"]);
  const info = new TextEncoder().encode(`blaze tenant key\0${tenantId}\0${row.key_version}`);
  const bits = await crypto.subtle.deriveBits({ name: "HKDF", hash: "SHA-256", salt: new Uint8Array(row.salt), info }, ikm, 256);
  const key = await crypto.subtle.importKey("raw", bits, { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
  tenantKeys.set(tenantId, { key, until: Date.now() + 60_000 });
  return key;
}

export async function keyIn(ns: Namespace, hash: Uint8Array): Promise<Uint8Array> {
  if (ns.key === null) return hash;
  return new Uint8Array(await crypto.subtle.sign("HMAC", ns.key, hash));
}

export interface Location {
  ns: string;
  object: string;
  offset: number;
  length: number;
  ulength: number;
  codec: number;
  /** The object's upload date, unix seconds (D1 `objects.uploaded`). */
  uploaded: number;
}

/**
 * For each hash, its location in the first namespace of `read` that has it, or null. `d1Ms` is
 * the time spent waiting on D1 (a Worker's clock moves only across IO, so CPU time is not
 * measurable here).
 */
export async function locate(env: CasEnv, read: Namespace[], hashes: Uint8Array[]): Promise<{ locations: (Location | null)[]; d1Ms: number }> {
  const keys = await Promise.all(read.map((ns) => Promise.all(hashes.map(async (h) => hex(await keyIn(ns, h))))));
  const live = clock.nowSeconds() - ROW_TTL;
  const statements = read.flatMap((ns, n) =>
    chunks(keys[n], ROWS_PER_STATEMENT).map((part) =>
      env.DB.prepare(
        `SELECT b.key, b.object, b."offset", b.length, b.ulength, b.codec, o.uploaded FROM blobs b JOIN objects o ON o.object = b.object
         WHERE b.ns = ?1 AND b.key IN (SELECT value FROM json_each(?2)) AND o.uploaded >= ?3`,
      ).bind(ns.ns, JSON.stringify(part), live),
    ),
  );
  const t0 = Date.now();
  const results = statements.length ? await env.DB.batch<Omit<Location, "ns"> & { key: string }>(statements) : [];
  const d1Ms = Date.now() - t0;
  const byNs = read.map(() => new Map<string, Omit<Location, "ns">>());
  let r = 0;
  for (const [n] of read.entries()) {
    for (let i = 0; i < chunks(keys[n], ROWS_PER_STATEMENT).length; i++) {
      for (const row of results[r++].results) byNs[n].set(row.key, row);
    }
  }
  const locations = hashes.map((_, i) => {
    for (const [n, ns] of read.entries()) {
      const row = byNs[n].get(keys[n][i]);
      if (row) return { ns: ns.ns, ...row };
    }
    return null;
  });
  return { locations, d1Ms };
}

function timing(d1Ms: number): Record<string, string> {
  return { "server-timing": `d1;dur=${d1Ms}` };
}

// ---- /has and /get

async function hashList(request: Request): Promise<Uint8Array[] | Response> {
  const declared = Number(request.headers.get("content-length") ?? "0");
  if (declared > MAX_HASHES * 40 + 64) return text(413, `at most ${MAX_HASHES} hashes`);
  let body: Cbor;
  try {
    body = decode(new Uint8Array(await request.arrayBuffer()));
  } catch (e) {
    if (e instanceof CborError || e instanceof TypeError) return text(400, `body: ${e.message}`);
    throw e;
  }
  const hashes = (body as { hashes?: Cbor })?.hashes;
  if (!Array.isArray(hashes) || hashes.length > MAX_HASHES || !hashes.every((h) => h instanceof Uint8Array && h.length === 32)) {
    return text(400, `body must be {"hashes": [32-byte strings]}, at most ${MAX_HASHES}`);
  }
  return hashes as Uint8Array[];
}

export async function has(env: CasEnv, request: Request, acc: Access): Promise<Answer> {
  const hashes = await hashList(request);
  if (hashes instanceof Response) return done(hashes);
  const own = acc.read[0].ns === "pub" ? null : acc.read[0].ns;
  const { locations, d1Ms } = await locate(env, acc.read, hashes);
  const state = locations.map((l) => (l === null ? 0 : l.ns === own ? 2 : 1));
  return { response: cbor({ state }, timing(d1Ms)), bytesIn: hashes.length * 32, bytesOut: state.length };
}

export async function get(env: CasEnv, request: Request, acc: Access): Promise<Answer> {
  const hashes = await hashList(request);
  if (hashes instanceof Response) return done(hashes);
  const { locations, d1Ms } = await locate(env, acc.read, hashes);

  const found = locations
    .map((loc, i) => ({ loc, hash: hashes[i] }))
    .filter((f): f is { loc: Location; hash: Uint8Array } => f.loc !== null)
    .sort((a, b) => nsRank(acc, a.loc.ns) - nsRank(acc, b.loc.ns) || cmp(a.loc.object, b.loc.object) || a.loc.offset - b.loc.offset);
  const missing = hashes.filter((_, i) => locations[i] === null);

  type Read = { object: string; start: number; end: number; items: typeof found };
  const reads: Read[] = [];
  for (const f of found) {
    const last = reads.at(-1);
    if (last && last.object === f.loc.object && f.loc.offset >= last.end && f.loc.offset + f.loc.length - last.start <= MAX_READ && f.loc.offset - last.end < 65536) {
      last.end = f.loc.offset + f.loc.length;
      last.items.push(f);
    } else {
      reads.push({ object: f.loc.object, start: f.loc.offset, end: f.loc.offset + f.loc.length, items: [f] });
    }
  }

  const { readable, writable } = new TransformStream<Uint8Array, Uint8Array>();
  const writer = writable.getWriter();
  const fetchRead = async (r: Read): Promise<Uint8Array> => {
    const obj = await env.BUCKET.get(r.object, { range: { offset: r.start, length: r.end - r.start } });
    if (obj === null) throw new Error(`a location names ${r.object}, which is gone`);
    const bytes = new Uint8Array(await obj.arrayBuffer());
    if (bytes.length !== r.end - r.start) throw new Error(`${r.object}: read ${bytes.length} of ${r.end - r.start} bytes`);
    return bytes;
  };
  const pump = (async () => {
    try {
      await writer.write(STREAM_MAGIC);
      const inflight: Promise<Uint8Array>[] = [];
      for (let i = 0; i < Math.min(READ_AHEAD, reads.length); i++) inflight.push(fetchRead(reads[i]));
      for (let i = 0; i < reads.length; i++) {
        const bytes = await inflight[i];
        if (i + READ_AHEAD < reads.length) inflight.push(fetchRead(reads[i + READ_AHEAD]));
        // One write per R2 read: thousands of small writes cost more than the bytes.
        const records = reads[i].items.map(({ loc, hash }) => blobRecordHeader(hash, loc.codec, null, loc.ulength, loc.length));
        const out = new Uint8Array(records.reduce((n, r) => n + r.length, 0) + (reads[i].end - reads[i].start));
        let pos = 0;
        for (const [j, { loc }] of reads[i].items.entries()) {
          out.set(records[j], pos);
          pos += records[j].length;
          out.set(bytes.subarray(loc.offset - reads[i].start, loc.offset - reads[i].start + loc.length), pos);
          pos += loc.length;
        }
        await writer.write(out.subarray(0, pos));
      }
      const tail = new Uint8Array(missing.length * 33 + 1);
      missing.forEach((h, i) => tail.set(missingRecord(h), i * 33));
      tail[tail.length - 1] = TAG_END;
      await writer.write(tail);
      await writer.close();
    } catch (e) {
      // The stream ends without its end record, which the client reports as a cut-short answer.
      await writer.abort(e);
    }
  })();
  const bytesOut = found.reduce((n, f) => n + f.loc.length + 42, 0);
  return {
    response: new Response(readable, { status: 200, headers: { "content-type": "application/vnd.blaze.pack-stream", ...timing(d1Ms) } }),
    bytesIn: hashes.length * 32,
    bytesOut,
    pump,
  };
}

function nsRank(acc: Access, ns: string): number {
  return acc.read.findIndex((n) => n.ns === ns);
}

// ---- /blob/<sha256>

export async function blob(env: CasEnv, request: Request, acc: Access, hashHex: string, headOnly: boolean, notFound: () => Response): Promise<Answer> {
  const {
    locations: [loc],
  } = await locate(env, acc.read, [unhex(hashHex)]);
  if (loc === null) return done(notFound());
  const headers = new Headers({
    "content-type": "application/octet-stream",
    "x-blaze-codec": loc.codec === CODEC_RAW ? "raw" : "zstd",
    "x-blaze-ulength": String(loc.ulength),
    "accept-ranges": "bytes",
    "cache-control": loc.ns === "pub" ? "public, max-age=31536000, immutable" : "private, no-cache",
  });
  const rangeHeader = headOnly ? null : request.headers.get("range");
  let start = 0;
  let end = loc.length - 1;
  if (rangeHeader !== null) {
    const m = /^bytes=(\d{1,15})-(\d{0,15})$/.exec(rangeHeader.trim());
    const a = m ? Number(m[1]) : NaN;
    const b = m && m[2] !== "" ? Number(m[2]) : loc.length - 1;
    if (!m || a >= loc.length || b < a) {
      headers.set("content-range", `bytes */${loc.length}`);
      return done(new Response("range not satisfiable\n", { status: 416, headers }));
    }
    start = a;
    end = Math.min(b, loc.length - 1);
    headers.set("content-range", `bytes ${start}-${end}/${loc.length}`);
  }
  headers.set("content-length", String(end - start + 1));
  if (headOnly) return done(new Response(null, { status: 200, headers }));
  const obj = await env.BUCKET.get(loc.object, { range: { offset: loc.offset + start, length: end - start + 1 } });
  if (obj === null) throw new Error(`a location names ${loc.object}, which is gone`);
  return {
    response: new Response(obj.body, { status: rangeHeader === null ? 200 : 206, headers }),
    bytesIn: 0,
    bytesOut: end - start + 1,
  };
}

// ---- /put

export async function put(env: CasEnv, request: Request, acc: Access): Promise<Answer> {
  if (acc.write === null) return done(text(403, "the token is read-only"));
  const ns = acc.write;
  const header = request.headers.get("content-length");
  if (header === null || !/^\d{1,12}$/.test(header)) return done(text(411, "Content-Length required"));
  const length = Number(header);
  if (length > MAX_PUT) return done(text(413, `a /put is at most ${MAX_PUT} bytes`));
  const bytes = new Uint8Array(await request.arrayBuffer());
  const answer = (response: Response) => ({ response, bytesIn: bytes.length, bytesOut: 0 });

  let frames;
  try {
    frames = parseStream(bytes);
  } catch (e) {
    if (e instanceof StreamError) return answer(text(400, `pack stream: ${e.message}`));
    throw e;
  }
  for (const f of frames) {
    const refusal = await check(f);
    if (refusal !== null) return answer(text(422, `${refusal}: ${hex(f.hash)}`));
  }

  const object = await storePack(env, ns, frames);
  return answer(cbor({ pack: object.slice(object.lastIndexOf("/") + 1, -".bpk".length), blobs: frames.length }));
}

/** Writes `frames` as one new pack in `ns` and points every blob's location at it. Returns the object's name. */
export async function storePack(env: CasEnv, ns: Namespace, frames: Frame[]): Promise<string> {
  const keys = await Promise.all(frames.map((f) => keyIn(ns, f.hash)));
  const pack = buildPack(frames, keys);
  const packSha = new Uint8Array(await crypto.subtle.digest("SHA-256", pack.bytes));
  const object = ns.key === null ? `cas/pub/${hex(packSha)}.bpk` : `cas/t/${ns.ns}/${hex(await keyIn(ns, packSha))}.bpk`;
  const stored = await env.BUCKET.put(object, pack.bytes, { sha256: hex(packSha) });

  const rows = pack.entries.map((e) => [hex(e.key), e.offset, e.clen, e.ulen, e.codec]);
  await env.DB.batch([
    env.DB.prepare("INSERT OR REPLACE INTO objects (object, uploaded) VALUES (?1, ?2)").bind(object, Math.floor(stored.uploaded.getTime() / 1000)),
    ...chunks(rows, ROWS_PER_STATEMENT).map((part) =>
      env.DB.prepare(
        `INSERT INTO blobs (ns, key, object, "offset", length, ulength, codec)
         SELECT ?1, json_extract(value, '$[0]'), ?2, json_extract(value, '$[1]'), json_extract(value, '$[2]'),
                json_extract(value, '$[3]'), json_extract(value, '$[4]')
         FROM json_each(?3) WHERE true
         ON CONFLICT (ns, key) DO UPDATE SET object = excluded.object, "offset" = excluded."offset",
           length = excluded.length, ulength = excluded.ulength, codec = excluded.codec, dict = NULL`,
      ).bind(ns.ns, object, JSON.stringify(part)),
    ),
  ]);
  return object;
}

/**
 * Deletes up to `SWEEP_ROWS` location rows whose object is past `ROW_TTL`, and the `objects` rows
 * left with no location. Run in the background of writes; there is no separate job.
 */
export async function sweep(env: CasEnv): Promise<void> {
  const cutoff = clock.nowSeconds() - ROW_TTL;
  await env.DB.batch([
    env.DB.prepare(
      `DELETE FROM blobs WHERE (ns, key) IN
         (SELECT b.ns, b.key FROM objects o JOIN blobs b ON b.object = o.object WHERE o.uploaded < ?1 LIMIT ?2)`,
    ).bind(cutoff, SWEEP_ROWS),
    env.DB.prepare(
      `DELETE FROM objects WHERE object IN
         (SELECT o.object FROM objects o WHERE o.uploaded < ?1 AND NOT EXISTS (SELECT 1 FROM blobs b WHERE b.object = o.object) LIMIT ?2)`,
    ).bind(cutoff, SWEEP_ROWS),
  ]);
}

/** Why a frame is refused, or null. Decompresses zstd with workerd's `node:zlib`, capped at the frame's `ulen`. */
async function check(f: { hash: Uint8Array; codec: number; dict: Uint8Array | null; ulen: number; data: Uint8Array }): Promise<string | null> {
  if (f.dict !== null) return "dictionaries are not accepted yet";
  if (f.ulen > MAX_ULEN) return `ulen over ${MAX_ULEN}`;
  let content: Uint8Array;
  if (f.codec === CODEC_RAW) {
    content = f.data;
  } else if (f.codec === CODEC_ZSTD) {
    try {
      content = zlib.zstdDecompressSync(f.data, { maxOutputLength: Math.max(f.ulen, 1) });
    } catch (e) {
      return e instanceof RangeError ? "decompresses past its ulen" : "not a zstd frame";
    }
  } else {
    return `codec ${f.codec}`;
  }
  if (content.length !== f.ulen) return `length ${content.length} is not its ulen ${f.ulen}`;
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", content));
  return digest.every((b, i) => b === f.hash[i]) ? null : "hash mismatch";
}

// ---- helpers

function done(response: Response): Answer {
  return { response, bytesIn: 0, bytesOut: 0 };
}

function text(status: number, reason: string): Response {
  return new Response(`${reason}\n`, { status, headers: { "content-type": "text/plain; charset=utf-8" } });
}

function cbor(value: Cbor, extra: Record<string, string> = {}): Response {
  return new Response(encode(value), { status: 200, headers: { "content-type": "application/cbor", ...extra } });
}

function chunks<T>(xs: T[], n: number): T[][] {
  const out: T[][] = [];
  for (let i = 0; i < xs.length; i += n) out.push(xs.slice(i, i + n));
  return out;
}

function cmp(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0;
}

export function hex(bytes: Uint8Array): string {
  let s = "";
  for (const b of bytes) s += b.toString(16).padStart(2, "0");
  return s;
}

export function unhex(s: string): Uint8Array {
  const out = new Uint8Array(s.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(s.slice(2 * i, 2 * i + 2), 16);
  return out;
}
