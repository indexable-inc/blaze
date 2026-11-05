// blaze-gateway v0: today's remote layout per tenant, behind auth, over the R2 binding (API-v0.md).
//
// The access decision is made from KV alone, before any R2 call, and every way a caller may not
// see an object ends in the same `notFound` as an absent one.

import { identify, lookupTenant, type TenantRecord } from "./auth";
import * as cas from "./cas";
import { contentAddressed, HEX64, multipartAllowed, parseName, validTenant, type Name } from "./names";
import * as tindex from "./tindex";

export interface Env {
  BUCKET: R2Bucket;
  TOKENS: KVNamespace;
  RL_TENANT: RateLimit;
  RL_ANON: RateLimit;
  METER: AnalyticsEngineDataset;
  DB: D1Database;
  [secret: `GATEWAY_SECRET_${number}`]: string | undefined;
}

export const MAX_BODY = 90 * 1024 * 1024;
const MAX_COMPLETE_BODY = 1024 * 1024;
/** The period of both rate limits in wrangler.jsonc. */
const RETRY_AFTER_SECONDS = 10;

export type Op =
  | "get"
  | "head"
  | "put"
  | "mp_start"
  | "mp_part"
  | "mp_complete"
  | "mp_abort"
  | "has"
  | "get_blobs"
  | "blob"
  | "blob_head"
  | "put_blobs"
  | tindex.IndexOp
  | "other";

const READS: ReadonlySet<Op> = new Set(["get", "head", "has", "get_blobs", "blob", "blob_head", "trust_get", "heads", "seg_get"]);
const INDEX_OPS: ReadonlySet<Op> = new Set(["trust_get", "trust_put", "heads", "seg_get", "seg_put", "head_put"]);
/** Writes that also delete a bounded number of expired location rows (cas.ts `sweep`). */
const SWEEPING: ReadonlySet<Op> = new Set(["put_blobs", "seg_put", "head_put"]);

interface Route {
  op: Op;
  tenant: string | null;
  name: string | null;
}

interface Outcome {
  response: Response;
  bytesIn: number;
  bytesOut: number;
  /** Work still writing a streamed answer. */
  pump?: Promise<void>;
}

export default {
  fetch(request: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
    return handle(request, env, ctx);
  },
} satisfies ExportedHandler<Env>;

export async function handle(request: Request, env: Env, ctx?: ExecutionContext): Promise<Response> {
  const url = new URL(request.url);
  const route = classify(request.method, url);
  const meter = { tenant: "-" };
  let out: Outcome;
  try {
    out = await serve(request, env, url, route, meter);
    if (out.pump) ctx?.waitUntil(out.pump);
    if (SWEEPING.has(route.op) && out.response.status < 300) {
      const swept = cas.sweep(env);
      if (ctx) ctx.waitUntil(swept);
      else await swept;
    }
  } catch (e) {
    out = done(text(500, `internal error: ${firstLine(e)}`));
  }
  env.METER.writeDataPoint({
    indexes: [meter.tenant],
    blobs: [meter.tenant, route.op],
    doubles: [out.response.status, out.bytesIn, out.bytesOut],
  });
  return out.response;
}

function classify(method: string, url: URL): Route {
  const blob = /^\/blob\/(.*)$/.exec(url.pathname);
  if (blob) return { op: method === "GET" ? "blob" : method === "HEAD" ? "blob_head" : "other", tenant: null, name: blob[1] };
  for (const [path, op] of [["/has", "has"], ["/get", "get_blobs"], ["/put", "put_blobs"]] as const) {
    if (url.pathname === path) return { op: method === "POST" ? op : "other", tenant: null, name: null };
  }
  const index = tindex.classifyIndex(method, url.pathname);
  if (index) return { op: index.op, tenant: index.tenant, name: index.arg };
  const m = /^\/t\/([^/]+)\/v3\/(.*)$/.exec(url.pathname);
  const q = url.searchParams;
  let op: Op = "other";
  if (method === "GET") op = "get";
  else if (method === "HEAD") op = "head";
  else if (method === "PUT") op = q.has("upload") ? "mp_part" : "put";
  else if (method === "POST") op = q.has("uploads") ? "mp_start" : q.has("upload") ? "mp_complete" : "other";
  else if (method === "DELETE") op = q.has("upload") ? "mp_abort" : "other";
  // A directory or the base itself is a listing, which v0 does not offer.
  if (m && (m[2] === "" || m[2].endsWith("/"))) op = "other";
  return { op, tenant: m ? m[1] : null, name: m ? m[2] : null };
}

async function serve(request: Request, env: Env, url: URL, route: Route, meter: { tenant: string }): Promise<Outcome> {
  const { op } = route;
  if (op === "other") {
    return done(text(405, "method not allowed", { allow: "GET, HEAD, PUT, POST" }));
  }

  const caller = await identify(request, env.TOKENS, Math.floor(Date.now() / 1000));
  const limit =
    caller.kind === "token"
      ? await env.RL_TENANT.limit({ key: `t:${caller.tenant}` })
      : await env.RL_ANON.limit({ key: `ip:${request.headers.get("cf-connecting-ip") ?? "unknown"}` });
  if (!limit.success) {
    return done(text(429, "rate limited", { "retry-after": String(RETRY_AFTER_SECONDS) }));
  }
  if (caller.kind === "rejected") return done(text(401, caller.reason));
  const writing = !READS.has(op);
  if (writing && caller.kind === "anonymous") return done(text(401, "a write needs a token"));

  if (op === "has" || op === "get_blobs" || op === "blob" || op === "blob_head" || op === "put_blobs") {
    if (caller.kind === "token") meter.tenant = caller.tenant;
    const acc = await cas.access(env, caller);
    if (acc instanceof Response) return done(acc);
    if (op === "has") return cas.has(env, request, acc);
    if (op === "get_blobs") return cas.get(env, request, acc);
    if (op === "put_blobs") return cas.put(env, request, acc);
    if (!/^[0-9a-f]{64}$/.test(route.name ?? "")) return done(notFound(op));
    return cas.blob(env, request, acc, route.name!, op === "blob_head", () => notFound(op));
  }

  if (INDEX_OPS.has(op)) {
    if (caller.kind === "token") meter.tenant = caller.tenant;
    return tindex.serve(env, request, op as tindex.IndexOp, route.tenant!, route.name, caller);
  }

  // Everything below is decided from KV only; R2 is not touched until access is granted.
  const tenant = route.tenant !== null && validTenant(route.tenant) ? await lookupTenant(env.TOKENS, route.tenant) : null;
  const name = route.name !== null ? parseName(route.name) : null;
  const own = caller.kind === "token" && caller.tenant === route.tenant;
  if (tenant !== null) meter.tenant = route.tenant!;

  if (!writing) {
    if (tenant === null || name === null || !(tenant.public || own)) return done(notFound(op));
    return read(env, request, op, objectKey(tenant, name), name, tenant);
  }
  if (tenant === null || !own) {
    return done(tenant?.public ? text(403, "the token is not for this tenant") : notFound(op));
  }
  if (caller.kind !== "token" || caller.scope !== "write") return done(text(403, "the token is read-only"));
  if (name === null) return done(notFound(op));

  const key = objectKey(tenant, name);
  switch (op) {
    case "put":
      return put(env, request, key, name);
    case "mp_start":
      return multipartStart(env, tenant, name);
    case "mp_part":
      return multipartPart(env, request, url, tenant, name);
    case "mp_complete":
      return multipartComplete(env, request, url, key, tenant, name);
    case "mp_abort":
      return multipartAbort(env, url, tenant, name);
    default:
      throw new Error(`write path reached for ${op}`);
  }
}

function objectKey(tenant: TenantRecord, name: Name): string {
  return `v3/${tenant.id}/${name.name}`;
}

/** Where a multipart upload is assembled before its hash is checked. Derived from the tenant and name, so an upload id cannot reach another tenant's staging. */
function stagingKey(tenant: TenantRecord, name: Name, nonce: string): string {
  return `staging/${tenant.id}/${name.name}/${nonce}`;
}

// ---- reads

async function read(env: Env, request: Request, op: Op, key: string, name: Name, tenant: TenantRecord): Promise<Outcome> {
  if (op === "head") {
    const obj = await env.BUCKET.head(key);
    if (obj === null) return done(notFound(op));
    const headers = objectHeaders(obj, name, tenant);
    headers.set("content-length", String(obj.size));
    return done(new Response(null, { status: 200, headers }));
  }

  const rangeHeader = request.headers.get("range");
  if (rangeHeader === null) {
    const obj = await env.BUCKET.get(key);
    if (obj === null) return done(notFound(op));
    const headers = objectHeaders(obj, name, tenant);
    headers.set("content-length", String(obj.size));
    return { response: new Response(obj.body, { status: 200, headers }), bytesIn: 0, bytesOut: obj.size };
  }

  const range = parseRange(rangeHeader);
  let obj: R2ObjectBody | null = null;
  if (range !== null) {
    try {
      obj = await env.BUCKET.get(key, {
        range: range.end === null ? { offset: range.start } : { offset: range.start, length: range.end - range.start + 1 },
      });
    } catch (e) {
      if (!/\(10039\)|range is not satisfiable/i.test(String(e))) throw e;
    }
    if (obj !== null && range.start < obj.size) {
      const end = Math.min(range.end ?? obj.size - 1, obj.size - 1);
      const length = end - range.start + 1;
      const headers = objectHeaders(obj, name, tenant);
      headers.set("content-length", String(length));
      headers.set("content-range", `bytes ${range.start}-${end}/${obj.size}`);
      return { response: new Response(obj.body, { status: 206, headers }), bytesIn: 0, bytesOut: length };
    }
    await obj?.body.cancel();
  }
  // Unsatisfiable or malformed: 416 names the size, which only an existing object has.
  const head = await env.BUCKET.head(key);
  if (head === null) return done(notFound(op));
  return done(text(416, "range not satisfiable", { "content-range": `bytes */${head.size}` }));
}

/** `bytes=a-` or `bytes=a-b` (API-v0.md). Anything else is unsatisfiable. */
export function parseRange(header: string): { start: number; end: number | null } | null {
  const m = /^bytes=(\d{1,15})-(\d{0,15})$/.exec(header.trim());
  if (!m) return null;
  const start = Number(m[1]);
  const end = m[2] === "" ? null : Number(m[2]);
  if (end !== null && end < start) return null;
  return { start, end };
}

function objectHeaders(obj: R2Object, name: Name, tenant: TenantRecord): Headers {
  const headers = new Headers({
    etag: obj.httpEtag,
    "last-modified": obj.uploaded.toUTCString(),
    "x-blaze-uploaded": String(Math.floor(obj.uploaded.getTime() / 1000)),
    "accept-ranges": "bytes",
    "content-type": "application/octet-stream",
    "cache-control": !tenant.public
      ? "private, no-cache"
      : contentAddressed(name.kind)
        ? "public, max-age=31536000, immutable"
        : "public, no-cache",
  });
  const sha = obj.customMetadata?.sha256;
  if (name.kind === "pack.zst" && sha !== undefined) headers.set("x-blaze-sha256", sha);
  return headers;
}

// ---- writes

async function put(env: Env, request: Request, key: string, name: Name): Promise<Outcome> {
  const length = bodyLength(request);
  if (length instanceof Response) return done(length);
  const expected = expectedSha256(request, name);
  if (expected instanceof Response) return done(expected);
  let obj: R2Object | null;
  try {
    obj = await env.BUCKET.put(key, request.body ?? new Uint8Array(0), {
      onlyIf: conditions(request),
      sha256: expected ?? undefined,
      customMetadata: name.kind === "pack.zst" ? { sha256: expected! } : undefined,
    });
  } catch (e) {
    const answer = r2ClientError(e);
    if (answer === null) throw e;
    return { response: answer, bytesIn: length, bytesOut: 0 };
  }
  if (obj === null) return { response: text(412, "precondition failed"), bytesIn: length, bytesOut: 0 };
  return { response: etagAnswer(obj), bytesIn: length, bytesOut: 0 };
}

async function multipartStart(env: Env, tenant: TenantRecord, name: Name): Promise<Outcome> {
  if (!multipartAllowed(name.kind)) return done(text(405, "multipart uploads are for packs/ and units/ only"));
  const nonce = randomHex(16);
  const upload = await env.BUCKET.createMultipartUpload(stagingKey(tenant, name, nonce));
  return done(json(200, { upload: `${nonce}.${base64url(upload.uploadId)}` }));
}

async function multipartPart(env: Env, request: Request, url: URL, tenant: TenantRecord, name: Name): Promise<Outcome> {
  if (!multipartAllowed(name.kind)) return done(text(405, "multipart uploads are for packs/ and units/ only"));
  const upload = parseUpload(url.searchParams.get("upload"));
  if (upload === null) return done(text(400, "bad upload id"));
  const partParam = url.searchParams.get("part") ?? "";
  const part = /^[1-9]\d{0,4}$/.test(partParam) ? Number(partParam) : 0;
  if (part < 1 || part > 10000) return done(text(400, "part must be 1 to 10000"));
  const length = bodyLength(request);
  if (length instanceof Response) return done(length);
  const mp = env.BUCKET.resumeMultipartUpload(stagingKey(tenant, name, upload.nonce), upload.uploadId);
  try {
    const uploaded = await mp.uploadPart(part, request.body ?? new Uint8Array(0));
    return { response: json(200, { etag: uploaded.etag }), bytesIn: length, bytesOut: 0 };
  } catch (e) {
    const answer = r2ClientError(e);
    if (answer === null) throw e;
    return { response: answer, bytesIn: length, bytesOut: 0 };
  }
}

interface CompleteBody {
  parts: { part: number; etag: string }[];
  sha256: string;
}

async function multipartComplete(
  env: Env,
  request: Request,
  url: URL,
  key: string,
  tenant: TenantRecord,
  name: Name,
): Promise<Outcome> {
  if (!multipartAllowed(name.kind)) return done(text(405, "multipart uploads are for packs/ and units/ only"));
  const upload = parseUpload(url.searchParams.get("upload"));
  if (upload === null) return done(text(400, "bad upload id"));
  const declared = Number(request.headers.get("content-length") ?? "0");
  if (declared > MAX_COMPLETE_BODY) return done(text(413, "completion body too large"));
  const raw = await request.text();
  const body = parseCompleteBody(raw);
  if (body === null) return done(text(400, 'body must be {"parts":[{"part":n,"etag":"..."}],"sha256":"<hex>"}'));
  const expected = expectedSha256(request, name);
  if (expected instanceof Response) return done(expected);

  const staging = stagingKey(tenant, name, upload.nonce);
  const mp = env.BUCKET.resumeMultipartUpload(staging, upload.uploadId);
  const bytesIn = raw.length;
  try {
    if (body.sha256 !== expected) {
      await mp.abort();
      return { response: text(422, "hash mismatch"), bytesIn, bytesOut: 0 };
    }
    await mp.complete(body.parts.map((p) => ({ partNumber: p.part, etag: p.etag })));
  } catch (e) {
    const answer = r2ClientError(e);
    if (answer === null) throw e;
    return { response: answer, bytesIn, bytesOut: 0 };
  }

  // The binding has no copy: the staged object is streamed back through the Worker into the
  // final name, and R2 checks the SHA-256 of what arrives (one extra read and write per upload).
  try {
    const staged = await env.BUCKET.get(staging);
    if (staged === null) throw new Error(`staged upload ${staging} is gone`);
    let obj: R2Object | null;
    try {
      obj = await env.BUCKET.put(key, staged.body.pipeThrough(new FixedLengthStream(staged.size)), {
        onlyIf: conditions(request),
        sha256: expected!,
        customMetadata: name.kind === "pack.zst" ? { sha256: expected! } : undefined,
      });
    } catch (e) {
      const answer = r2ClientError(e);
      if (answer === null) throw e;
      return { response: answer, bytesIn, bytesOut: 0 };
    }
    if (obj === null) return { response: text(412, "precondition failed"), bytesIn, bytesOut: 0 };
    return { response: etagAnswer(obj), bytesIn, bytesOut: 0 };
  } finally {
    await env.BUCKET.delete(staging);
  }
}

async function multipartAbort(env: Env, url: URL, tenant: TenantRecord, name: Name): Promise<Outcome> {
  if (!multipartAllowed(name.kind)) return done(text(405, "multipart uploads are for packs/ and units/ only"));
  const upload = parseUpload(url.searchParams.get("upload"));
  if (upload === null) return done(text(400, "bad upload id"));
  try {
    await env.BUCKET.resumeMultipartUpload(stagingKey(tenant, name, upload.nonce), upload.uploadId).abort();
  } catch (e) {
    const answer = r2ClientError(e);
    if (answer === null) throw e;
    return done(answer);
  }
  return done(new Response(null, { status: 204 }));
}

function parseCompleteBody(raw: string): CompleteBody | null {
  let v: unknown;
  try {
    v = JSON.parse(raw);
  } catch {
    return null;
  }
  if (typeof v !== "object" || v === null) return null;
  const { parts, sha256 } = v as Record<string, unknown>;
  if (typeof sha256 !== "string" || !HEX64.test(sha256) || !Array.isArray(parts) || parts.length === 0) return null;
  for (const p of parts) {
    if (typeof p !== "object" || p === null) return null;
    const { part, etag } = p as Record<string, unknown>;
    if (!Number.isInteger(part) || (part as number) < 1 || (part as number) > 10000 || typeof etag !== "string") return null;
  }
  return { parts: parts as CompleteBody["parts"], sha256 };
}

/** The SHA-256 R2 must see for this name's body: the name's own hash, or `x-blaze-sha256` for `.pack.zst`. */
function expectedSha256(request: Request, name: Name): string | null | Response {
  if (!contentAddressed(name.kind)) return null;
  if (name.kind !== "pack.zst") return name.hash;
  const sha = request.headers.get("x-blaze-sha256");
  if (sha === null || !HEX64.test(sha)) return text(400, "a .pack.zst needs x-blaze-sha256: the lowercase hex SHA-256 of the body");
  return sha;
}

function bodyLength(request: Request): number | Response {
  const header = request.headers.get("content-length");
  if (header === null || !/^\d{1,12}$/.test(header)) return text(411, "Content-Length required");
  const length = Number(header);
  if (length > MAX_BODY) return text(413, `body over ${MAX_BODY} bytes`);
  return length;
}

/** `If-Match: <etag>` and `If-None-Match: *`, passed to R2's own conditional put so the check and the write are one step. */
function conditions(request: Request): Headers | undefined {
  const ifMatch = request.headers.get("if-match");
  const ifNoneMatch = request.headers.get("if-none-match");
  if (ifMatch === null && ifNoneMatch === null) return undefined;
  const headers = new Headers();
  if (ifMatch !== null) headers.set("if-match", quoteEtags(ifMatch));
  if (ifNoneMatch !== null) headers.set("if-none-match", ifNoneMatch.trim() === "*" ? "*" : quoteEtags(ifNoneMatch));
  return headers;
}

/** The client may echo the JSON `etag` (bare) or the `ETag` header (quoted); R2 wants quoted strong tags. */
function quoteEtags(list: string): string {
  return list
    .split(",")
    .map((t) => t.trim().replace(/^W\//, "").replace(/^"(.*)"$/, "$1"))
    .filter((t) => t !== "")
    .map((t) => `"${t}"`)
    .join(", ");
}

/** R2 errors a client caused, as the answer to give; null for anything else. */
function r2ClientError(e: unknown): Response | null {
  const msg = String(e);
  if (/\(10037\)|checksum you specified did not match/i.test(msg)) return text(422, "hash mismatch");
  if (/\(10024\)|multipart upload does not exist/i.test(msg)) return text(400, "no such upload");
  if (/\(100(11|25|48)\)|parts could not be found|smaller than the minimum|part.*size/i.test(msg)) {
    return text(400, `bad multipart upload: ${firstLine(e)}`);
  }
  if (/\(10031\)|pre-conditions you specified did not hold/i.test(msg)) return text(412, "precondition failed");
  return null;
}

function parseUpload(id: string | null): { nonce: string; uploadId: string } | null {
  if (id === null) return null;
  const m = /^([0-9a-f]{32})\.([A-Za-z0-9_-]{1,2048})$/.exec(id);
  if (!m) return null;
  try {
    return { nonce: m[1], uploadId: unbase64url(m[2]) };
  } catch {
    return null;
  }
}

// ---- small helpers

function done(response: Response): Outcome {
  return { response, bytesIn: 0, bytesOut: 0 };
}

/** The gateway's one 404: the same status, body and headers for every reason. */
function notFound(op: Op): Response {
  return text(404, "not found", {}, op === "head" || op === "blob_head");
}

function text(status: number, reason: string, extra: Record<string, string> = {}, noBody = false): Response {
  return new Response(noBody ? null : `${reason}\n`, {
    status,
    headers: { "content-type": "text/plain; charset=utf-8", ...extra },
  });
}

function json(status: number, value: unknown, extra: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(value), { status, headers: { "content-type": "application/json", ...extra } });
}

function etagAnswer(obj: R2Object): Response {
  return json(200, { etag: obj.etag }, { etag: obj.httpEtag });
}

function firstLine(e: unknown): string {
  return String(e instanceof Error ? e.message : e).split("\n")[0];
}

function randomHex(bytes: number): string {
  return [...crypto.getRandomValues(new Uint8Array(bytes))].map((b) => b.toString(16).padStart(2, "0")).join("");
}

function base64url(s: string): string {
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function unbase64url(s: string): string {
  return atob(s.replace(/-/g, "+").replace(/_/g, "/"));
}
