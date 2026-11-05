// The signed index (API.md, "Index"): per-tenant key lists, heads and segments, the checks on
// every index write, and the refresh inside head writes (docs/remote-cache.md, "GC").
//
// A tenant's index is readable by public tenants' readers and by its own tokens; to everyone
// else a private tenant's trust list, heads and segments are the one 404 an absent one is.

import { lookupTenant, type Caller, type TenantRecord } from "./auth";
import * as cas from "./cas";
import { CborError, decode, encode, type Cbor } from "./cbor";
import { clock, DAY } from "./clock";
import { HEX64, validTenant } from "./names";
import type { Frame } from "./pack";
import * as schema from "./schema";
import { SchemaError } from "./schema";

export type IndexOp = "trust_get" | "trust_put" | "heads" | "seg_get" | "seg_put" | "head_put";

export const INDEX_READS: ReadonlySet<string> = new Set(["trust_get", "heads", "seg_get"]);

/** A segment older than this is rewritten by a head write that names it. */
export const SEGMENT_REFRESH_AGE = 2 * DAY;
/** A pack older than this is rewritten (or, for a private head's public blob, copied) by a head write that names one of its blobs. */
export const PACK_REFRESH_AGE = 45 * DAY;
/** Rewrites and copies one head write does at most; past it the write is refused with 503 and, sent again, goes on where it stopped. */
export const MAX_REFRESH_WRITES = 64;
export const MAX_HEAD_NAMES = 500;
/** A head's `created` may be this far ahead of the gateway's clock. */
const CLOCK_SLACK = 300;
const COPY_PACK_BYTES = 32 * 1024 * 1024;
const MAX_READ = 4 * 1024 * 1024;
const SEGMENT_READS = 4;
const PACK_REWRITES = 4;

export type IndexEnv = cas.CasEnv;

export interface Answer {
  response: Response;
  bytesIn: number;
  bytesOut: number;
}

export function classifyIndex(method: string, path: string): { op: IndexOp | "other"; tenant: string; arg: string | null } | null {
  const m = /^\/t\/([^/]+)\/(trust|heads|seg|head)(?:\/(.+))?$/.exec(path);
  if (!m) return null;
  const [, tenant, what, arg = null] = m;
  let op: IndexOp | "other" = "other";
  if (what === "trust" && arg === null) op = method === "GET" ? "trust_get" : method === "PUT" ? "trust_put" : "other";
  else if (what === "heads" && arg === null && method === "POST") op = "heads";
  else if (what === "seg" && arg !== null && method === "GET") op = "seg_get";
  else if (what === "seg" && arg === null && method === "PUT") op = "seg_put";
  else if (what === "head" && arg !== null && method === "PUT") op = "head_put";
  return { op, tenant, arg };
}

export async function serve(env: IndexEnv, request: Request, op: IndexOp, tenantName: string, arg: string | null, caller: Caller): Promise<Answer> {
  const tenant = validTenant(tenantName) ? await lookupTenant(env.TOKENS, tenantName) : null;
  const own = caller.kind === "token" && caller.tenant === tenantName;
  if (INDEX_READS.has(op)) {
    if (tenant === null || !(tenant.public || own)) return done(notFound());
    if (op === "trust_get") return trustGet(env, tenant);
    if (op === "seg_get") return segGet(env, tenant, arg!);
    return heads(env, request, tenant);
  }
  if (tenant === null || !own) return done(tenant?.public ? text(403, "the token is not for this tenant") : notFound());
  if (caller.kind !== "token" || caller.scope !== "write") return done(text(403, "the token is read-only"));
  try {
    if (op === "trust_put") return await trustPut(env, request, tenantName, tenant);
    if (op === "seg_put") return await segPut(env, request, tenant);
    return await headPut(env, request, tenantName, tenant, arg!);
  } catch (e) {
    if (e instanceof SchemaError) return done(text(400, e.message));
    throw e;
  }
}

// ---- names in the bucket

const HEAD_NAME = /^(p\/([A-Za-z0-9._-]{1,128})\/([0-9a-f]{7,64}|latest)|u\/[0-9a-f]{64})$/;

export function validHeadName(name: string): boolean {
  const m = HEAD_NAME.exec(name);
  return m !== null && m[2] !== "." && m[2] !== "..";
}

function trustKey(tenant: TenantRecord): string {
  return `trust/${tenant.id}/keys`;
}

/** A private tenant's names after its id are HMACs under `k_t`, with a domain prefix per kind. */
async function privateName(env: IndexEnv, tenant: TenantRecord, domain: string, value: Uint8Array): Promise<string> {
  const ns = await cas.namespaceOf(env, tenant);
  const prefix = new TextEncoder().encode(`blaze ${domain}\0`);
  const msg = new Uint8Array(prefix.length + value.length);
  msg.set(prefix, 0);
  msg.set(value, prefix.length);
  return cas.hex(new Uint8Array(await crypto.subtle.sign("HMAC", ns.key!, msg)));
}

async function headKey(env: IndexEnv, tenant: TenantRecord, name: string): Promise<string> {
  const leaf = tenant.public ? name : await privateName(env, tenant, "head", new TextEncoder().encode(name));
  return `index/heads/${tenant.id}/${leaf}`;
}

async function segKey(env: IndexEnv, tenant: TenantRecord, sha: Uint8Array): Promise<string> {
  const leaf = tenant.public ? cas.hex(sha) : await privateName(env, tenant, "seg", sha);
  return `index/seg/${tenant.id}/${leaf}`;
}

// ---- trust

async function trustGet(env: IndexEnv, tenant: TenantRecord): Promise<Answer> {
  const obj = await env.BUCKET.get(trustKey(tenant));
  if (obj === null) return done(notFound());
  const bytes = new Uint8Array(await obj.arrayBuffer());
  return out(cborBytes(bytes, { etag: obj.httpEtag, "cache-control": tenant.public ? "public, max-age=60" : "private, no-cache" }), 0, bytes.length);
}

async function trustPut(env: IndexEnv, request: Request, tenantName: string, tenant: TenantRecord): Promise<Answer> {
  if (!tenant.root) {
    return done(
      text(
        403,
        `tenant ${tenantName} has no root key registered with the gateway, so no key list can be checked; ` +
          `the holder of its root key registers it (docs/remote-cache.md, "The keys")`,
      ),
    );
  }
  const raw = await readBody(request, schema.MAX_KEYLIST_BYTES);
  if (raw instanceof Response) return done(raw);
  const answer = (r: Response) => out(r, raw.length, 0);
  const envelope = schema.parseEnvelope(raw);
  const list = schema.parseKeyList(envelope.body);
  if (list.tenant !== tenantName) return answer(text(422, `the key list is signed for tenant ${list.tenant}, not ${tenantName}`));
  if (list.signer !== tenant.root.name) return answer(text(422, `the key list names signer ${list.signer}; this tenant's root key is ${tenant.root.name}`));
  if (!(await schema.verify(cas.unhex(tenant.root.key), schema.KEYS_CONTEXT, envelope))) {
    return answer(text(422, `bad signature: the key list does not verify against root key ${tenant.root.name}`));
  }
  const key = trustKey(tenant);
  const stored = await env.BUCKET.get(key);
  let onlyIf: Headers;
  if (stored !== null) {
    const storedTurn = schema.parseKeyList(schema.parseEnvelope(new Uint8Array(await stored.arrayBuffer())).body).turn;
    if (list.turn <= storedTurn) return answer(text(409, `turn ${list.turn} is not above the stored key list's turn ${storedTurn}`));
    onlyIf = new Headers({ "if-match": stored.httpEtag });
  } else {
    onlyIf = new Headers({ "if-none-match": "*" });
  }
  const obj = await env.BUCKET.put(key, raw, { onlyIf });
  if (obj === null) return answer(text(412, "the key list changed while this one was checked; read it again"));
  return answer(cbor({ etag: obj.etag, turn: list.turn }, { etag: obj.httpEtag }));
}

async function keyList(env: IndexEnv, tenant: TenantRecord): Promise<schema.KeyList | null> {
  const obj = await env.BUCKET.get(trustKey(tenant));
  if (obj === null) return null;
  return schema.parseKeyList(schema.parseEnvelope(new Uint8Array(await obj.arrayBuffer())).body);
}

// ---- segments

async function segGet(env: IndexEnv, tenant: TenantRecord, arg: string): Promise<Answer> {
  if (!HEX64.test(arg)) return done(notFound());
  const obj = await env.BUCKET.get(await segKey(env, tenant, cas.unhex(arg)));
  if (obj === null) return done(notFound());
  const bytes = new Uint8Array(await obj.arrayBuffer());
  const cache = tenant.public ? "public, max-age=604800, immutable" : "private, no-cache";
  return out(cborBytes(bytes, { etag: obj.httpEtag, "cache-control": cache }), 0, bytes.length);
}

async function segPut(env: IndexEnv, request: Request, tenant: TenantRecord): Promise<Answer> {
  const raw = await readBody(request, schema.MAX_SEGMENT_BYTES);
  if (raw instanceof Response) return done(raw);
  const answer = (r: Response) => out(r, raw.length, 0);
  const entries = schema.parseSegment(raw);
  const named = new Map<string, { hash: Uint8Array; ulen: number }>();
  for (const e of entries) for (const b of e.blobs) named.set(cas.hex(b.hash), b);
  const blobs = [...named.values()];
  const acc = await cas.accessOf(env, tenant, true);
  const { locations, d1Ms } = await cas.locate(env, acc.read, blobs.map((b) => b.hash));
  for (const [i, loc] of locations.entries()) {
    if (loc === null) return answer(text(422, `the segment names a blob this tenant cannot read: ${cas.hex(blobs[i].hash)}`));
    if (loc.ulength !== blobs[i].ulen) {
      return answer(text(422, `the segment gives blob ${cas.hex(blobs[i].hash)} ulen ${blobs[i].ulen}; it is ${loc.ulength}`));
    }
  }
  const sha = new Uint8Array(await crypto.subtle.digest("SHA-256", raw));
  // Put only if absent: a segment's name is its content, so an existing one is this one.
  await env.BUCKET.put(await segKey(env, tenant, sha), raw, { sha256: cas.hex(sha), onlyIf: new Headers({ "if-none-match": "*" }) });
  return answer(cbor({ seg: sha }, { "server-timing": `d1;dur=${d1Ms}` }));
}

// ---- heads

async function heads(env: IndexEnv, request: Request, tenant: TenantRecord): Promise<Answer> {
  const raw = await readBody(request, MAX_HEAD_NAMES * 160 + 64);
  if (raw instanceof Response) return done(raw);
  let body: Cbor;
  try {
    body = decode(raw);
  } catch (e) {
    if (e instanceof CborError || e instanceof TypeError) return out(text(400, `body: ${e.message}`), raw.length, 0);
    throw e;
  }
  const names = (body as { names?: Cbor })?.names;
  if (!Array.isArray(names) || names.length > MAX_HEAD_NAMES || !names.every((n) => typeof n === "string" && validHeadName(n))) {
    return out(text(400, `body must be {"names": [head names]}, at most ${MAX_HEAD_NAMES}`), raw.length, 0);
  }
  const found = await Promise.all(
    (names as string[]).map(async (n) => {
      const obj = await env.BUCKET.get(await headKey(env, tenant, n));
      return obj === null ? null : { etag: obj.etag, head: new Uint8Array(await obj.arrayBuffer()) };
    }),
  );
  const bytesOut = found.reduce((n, f) => n + (f?.head.length ?? 0), 0);
  return out(cbor({ heads: found }, { "cache-control": "private, no-cache" }), raw.length, bytesOut);
}

async function headPut(env: IndexEnv, request: Request, tenantName: string, tenant: TenantRecord, name: string): Promise<Answer> {
  if (!validHeadName(name)) return done(text(400, "a head is named p/<project>/<commit>, p/<project>/latest or u/<unit key>"));
  const ifMatch = request.headers.get("if-match");
  const ifNoneMatch = request.headers.get("if-none-match");
  if ((ifMatch === null) === (ifNoneMatch === null) || (ifNoneMatch !== null && ifNoneMatch.trim() !== "*")) {
    return done(text(428, "a head write needs If-Match: <the ETag read> or If-None-Match: * for a new head"));
  }
  const raw = await readBody(request, schema.MAX_HEAD_BYTES);
  if (raw instanceof Response) return done(raw);
  const answer = (r: Response) => out(r, raw.length, 0);
  const envelope = schema.parseEnvelope(raw);
  const head = schema.parseHead(envelope.body);
  if (head.tenant !== tenantName) return answer(text(422, `the head is signed for tenant ${head.tenant}, not ${tenantName}`));
  if (head.name !== name) return answer(text(422, `the head is signed for ${head.name}, not ${name}`));
  const now = clock.nowSeconds();
  if (head.created > now + CLOCK_SLACK) return answer(text(422, `created ${head.created} is ahead of the gateway's clock (${now})`));

  const hk = await headKey(env, tenant, name);
  const [list, cur] = await Promise.all([keyList(env, tenant), ifMatch !== null ? env.BUCKET.get(hk) : env.BUCKET.head(hk)]);
  const refuse = async (r: Response) => {
    if (cur !== null && "body" in cur) await (cur as R2ObjectBody).body.cancel();
    return answer(r);
  };
  if (list === null) return refuse(text(422, `tenant ${tenantName} has no key list: PUT /t/${tenantName}/trust first`));
  const key = list.keys.find((k) => k.name === head.signer);
  if (key === undefined) {
    return refuse(text(422, `signer ${head.signer} is not in the tenant's key list (turn ${list.turn}): unknown or revoked`));
  }
  if (now > key.not_after) return refuse(text(422, `key ${key.name} expired at ${key.not_after}`));
  if (now < key.not_before) return refuse(text(422, `key ${key.name} is valid from ${key.not_before}`));
  if (head.created < key.not_before || head.created > key.not_after) {
    return refuse(text(422, `created ${head.created} is outside key ${key.name}'s window ${key.not_before}..${key.not_after}`));
  }
  if (!(await schema.verify(key.key, schema.HEAD_CONTEXT, envelope))) return refuse(text(422, `bad signature: the head does not verify against key ${key.name}`));

  let onlyIf: Headers;
  if (ifMatch !== null) {
    if (cur === null || !etagList(ifMatch).includes(cur.etag)) {
      return refuse(text(412, "the head is not at the ETag given: read it again, merge, sign and retry"));
    }
    const curTurn = schema.parseHead(schema.parseEnvelope(new Uint8Array(await (cur as R2ObjectBody).arrayBuffer())).body).turn;
    if (head.turn <= curTurn) return answer(text(409, `turn ${head.turn} is not above the stored head's turn ${curTurn}`));
    onlyIf = new Headers({ "if-match": cur.httpEtag });
  } else {
    if (cur !== null) return answer(text(412, "the head exists: read it and write with If-Match"));
    onlyIf = new Headers({ "if-none-match": "*" });
  }

  const t0 = Date.now();
  const refreshed = await refresh(env, tenant, head.segments);
  if (refreshed instanceof Response) return answer(refreshed);
  const refreshMs = Date.now() - t0;
  const obj = await env.BUCKET.put(hk, raw, { onlyIf });
  if (obj === null) return answer(text(412, "the head changed while this write was checked: read it again, merge, sign and retry"));
  return answer(cbor({ etag: obj.etag, refreshed }, { etag: obj.httpEtag, "server-timing": `refresh;dur=${refreshMs}` }));
}

function etagList(header: string): string[] {
  return header
    .split(",")
    .map((t) => t.trim().replace(/^W\//, "").replace(/^"(.*)"$/, "$1"))
    .filter((t) => t !== "");
}

// ---- the refresh (docs/remote-cache.md, "GC")

export interface Refreshed {
  /** Segments rewritten. */
  segments: number;
  /** Packs rewritten. */
  packs: number;
  /** Public blobs copied into a private tenant's own namespace. */
  copied: number;
  [k: string]: number;
}

/**
 * Makes every object the head names young enough to outlive it: each named segment older than
 * `SEGMENT_REFRESH_AGE` and each pack holding a named blob older than `PACK_REFRESH_AGE` is
 * written again through the binding (which has no copy), and its new upload date checked with a
 * HEAD. A private head never refreshes a public pack: the blobs it names there are copied into the
 * tenant's own namespace, where its reads look first.
 */
async function refresh(env: IndexEnv, tenant: TenantRecord, segments: Uint8Array[]): Promise<Refreshed | Response> {
  const acc = await cas.accessOf(env, tenant, true);
  const own = acc.write!;
  const now = clock.nowSeconds();
  const done: Refreshed = { segments: 0, packs: 0, copied: 0 };
  let budget = MAX_REFRESH_WRITES;
  let left = 0;

  const named = new Map<string, Uint8Array>();
  // A few segments at a time: at most 4 MiB each in memory.
  for (let i = 0; i < segments.length; i += SEGMENT_READS) {
    const batch = segments.slice(i, i + SEGMENT_READS);
    const read = await Promise.all(
      batch.map(async (seg) => {
        const key = await segKey(env, tenant, seg);
        const obj = await env.BUCKET.get(key);
        return { seg, key, obj, raw: obj === null ? null : new Uint8Array(await obj.arrayBuffer()) };
      }),
    );
    const absent = read.find((r) => r.obj === null);
    if (absent) return text(422, `the head names segment ${cas.hex(absent.seg)}, which this tenant does not have`);
    for (const { seg, key, obj, raw } of read) {
      for (const e of schema.parseSegment(raw!)) for (const b of e.blobs) named.set(cas.hex(b.hash), b.hash);
      if (now - seconds(obj!.uploaded) <= SEGMENT_REFRESH_AGE) continue;
      if (budget === 0) {
        left++;
        continue;
      }
      budget--;
      await env.BUCKET.put(key, raw!, { sha256: cas.hex(seg) });
      await movedSince(env, key, obj!);
      done.segments++;
    }
  }

  const hashes = [...named.values()];
  const { locations } = await cas.locate(env, acc.read, hashes);
  const old = new Map<string, { loc: cas.Location; hash: Uint8Array }[]>();
  for (const [i, loc] of locations.entries()) {
    if (loc === null) return text(422, `a segment the head names names blob ${cas.hex(hashes[i])}, which this tenant cannot read`);
    if (now - loc.uploaded <= PACK_REFRESH_AGE) continue;
    const list = old.get(loc.object) ?? [];
    list.push({ loc, hash: hashes[i] });
    old.set(loc.object, list);
  }

  const toCopy: { loc: cas.Location; hash: Uint8Array }[] = [];
  const toRewrite: string[] = [];
  for (const [object, items] of old) {
    if (items[0].loc.ns !== own.ns) toCopy.push(...items);
    else if (budget === 0) left++;
    else {
      budget--;
      toRewrite.push(object);
    }
  }
  // Streamed, not buffered, so several at once cost no memory.
  for (let i = 0; i < toRewrite.length; i += PACK_REWRITES) {
    await Promise.all(toRewrite.slice(i, i + PACK_REWRITES).map((object) => rewritePack(env, object)));
  }
  done.packs += toRewrite.length;

  if (toCopy.length > 0) {
    toCopy.sort((a, b) => (a.loc.object < b.loc.object ? -1 : a.loc.object > b.loc.object ? 1 : a.loc.offset - b.loc.offset));
    let frames: Frame[] = [];
    let size = 0;
    const flush = async () => {
      if (frames.length === 0) return;
      await cas.storePack(env, own, frames);
      done.copied += frames.length;
      frames = [];
      size = 0;
    };
    for (const run of runs(toCopy)) {
      if (budget === 0) {
        left += run.items.length;
        continue;
      }
      const obj = await env.BUCKET.get(run.object, { range: { offset: run.start, length: run.end - run.start } });
      if (obj === null) throw new Error(`a location names ${run.object}, which is gone`);
      const bytes = new Uint8Array(await obj.arrayBuffer());
      for (const { loc, hash } of run.items) {
        frames.push({ hash, codec: loc.codec, dict: null, ulen: loc.ulength, data: bytes.slice(loc.offset - run.start, loc.offset - run.start + loc.length) });
        size += loc.length;
      }
      if (size >= COPY_PACK_BYTES) {
        budget--;
        await flush();
      }
    }
    if (frames.length > 0) {
      budget--;
      await flush();
    }
  }

  if (left > 0) {
    return text(503, `refresh not finished: ${left} objects left after ${MAX_REFRESH_WRITES} writes; send the same request again`, {
      "retry-after": "1",
    });
  }
  return done;
}

function runs(items: { loc: cas.Location; hash: Uint8Array }[]) {
  const out: { object: string; start: number; end: number; items: typeof items }[] = [];
  for (const it of items) {
    const last = out.at(-1);
    const end = it.loc.offset + it.loc.length;
    if (last && last.object === it.loc.object && it.loc.offset >= last.end && it.loc.offset - last.end < 65536 && end - last.start <= MAX_READ) {
      last.end = end;
      last.items.push(it);
    } else {
      out.push({ object: it.loc.object, start: it.loc.offset, end, items: [it] });
    }
  }
  return out;
}

/** Streams a pack back onto its own name (R2 checks it against its MD5), then records the new upload date. */
async function rewritePack(env: IndexEnv, object: string): Promise<void> {
  const obj = await env.BUCKET.get(object);
  if (obj === null) throw new Error(`a location names ${object}, which is gone`);
  const md5 = /^[0-9a-f]{32}$/.test(obj.etag) ? obj.etag : undefined;
  const sha = /^cas\/pub\/([0-9a-f]{64})\.bpk$/.exec(object)?.[1];
  await env.BUCKET.put(object, obj.body.pipeThrough(new FixedLengthStream(obj.size)), sha ? { sha256: sha } : { md5 });
  const uploaded = await movedSince(env, object, obj);
  await env.DB.prepare("UPDATE objects SET uploaded = ?2 WHERE object = ?1").bind(object, seconds(uploaded)).run();
}

/** The HEAD after a rewrite: the upload date must have moved and the bytes stayed. */
async function movedSince(env: IndexEnv, key: string, before: R2Object): Promise<Date> {
  const after = await env.BUCKET.head(key);
  if (after === null || after.size !== before.size || after.etag !== before.etag) {
    throw new Error(`refresh of ${key}: the rewritten object is ${after === null ? "gone" : "not the same bytes"}`);
  }
  if (after.uploaded.getTime() <= before.uploaded.getTime()) {
    throw new Error(`refresh of ${key}: its upload date did not move (${before.uploaded.toISOString()})`);
  }
  return after.uploaded;
}

function seconds(d: Date): number {
  return Math.floor(d.getTime() / 1000);
}

// ---- helpers

async function readBody(request: Request, max: number): Promise<Uint8Array | Response> {
  const declared = request.headers.get("content-length");
  if (declared !== null && Number(declared) > max) return text(413, `body over ${max} bytes`);
  const bytes = new Uint8Array(await request.arrayBuffer());
  if (bytes.length > max) return text(413, `body over ${max} bytes`);
  return bytes;
}

function done(response: Response): Answer {
  return { response, bytesIn: 0, bytesOut: 0 };
}

function out(response: Response, bytesIn: number, bytesOut: number): Answer {
  return { response, bytesIn, bytesOut };
}

/** The same 404 as every other route's (index.ts `notFound`). */
function notFound(): Response {
  return text(404, "not found");
}

function text(status: number, reason: string, extra: Record<string, string> = {}): Response {
  return new Response(`${reason}\n`, { status, headers: { "content-type": "text/plain; charset=utf-8", ...extra } });
}

function cbor(value: Cbor, extra: Record<string, string> = {}): Response {
  return cborBytes(encode(value), extra);
}

function cborBytes(bytes: Uint8Array, extra: Record<string, string>): Response {
  return new Response(bytes, { status: 200, headers: { "content-type": "application/cbor", ...extra } });
}
