// The signed index's bodies (API.md, "Index"): key lists, heads and segments, deterministic CBOR,
// and the Ed25519 checks over them. Parsing is strict: a map with a missing, extra or mistyped
// field is refused, so a client and the gateway can never read one body two ways.

import { CborError, decode, type Cbor } from "./cbor";

export class SchemaError extends Error {}

export const HEAD_CONTEXT = new TextEncoder().encode("blaze head v1\0");
export const KEYS_CONTEXT = new TextEncoder().encode("blaze keys v1\0");

export const MAX_KEYS = 32;
export const MAX_SEGMENTS = 128;
export const MAX_SEGMENT_BYTES = 4 * 1024 * 1024;
export const MAX_HEAD_BYTES = 64 * 1024;
export const MAX_KEYLIST_BYTES = 16 * 1024;
const MAX_TEXT = 256;

export interface Envelope {
  body: Uint8Array;
  sig: Uint8Array;
}

export interface Key {
  name: string;
  key: Uint8Array;
  role: string;
  not_before: number;
  not_after: number;
}

export interface KeyList {
  tenant: string;
  turn: number;
  signer: string;
  keys: Key[];
}

export interface Head {
  tenant: string;
  name: string;
  turn: number;
  created: number;
  versions: Record<string, number>;
  toolchain: string;
  target: string;
  segments: Uint8Array[];
  signer: string;
}

export interface Entry {
  kind: string;
  key: Uint8Array;
  blobs: { hash: Uint8Array; ulen: number }[];
  data: Uint8Array;
}

function cbor(bytes: Uint8Array): Cbor {
  try {
    return decode(bytes);
  } catch (e) {
    if (e instanceof CborError || e instanceof TypeError) throw new SchemaError(e.message);
    throw e;
  }
}

function map(v: Cbor, what: string, fields: string[]): Record<string, Cbor> {
  if (typeof v !== "object" || v === null || Array.isArray(v) || v instanceof Uint8Array) throw new SchemaError(`${what} must be a map`);
  const keys = Object.keys(v).sort();
  const want = [...fields].sort();
  if (keys.length !== want.length || keys.some((k, i) => k !== want[i])) {
    throw new SchemaError(`${what} must have exactly the fields ${want.join(", ")}`);
  }
  return v as Record<string, Cbor>;
}

function uint(v: Cbor, what: string): number {
  if (typeof v !== "number") throw new SchemaError(`${what} must be an unsigned integer`);
  return v;
}

function textField(v: Cbor, what: string): string {
  if (typeof v !== "string" || v.length === 0 || v.length > MAX_TEXT) throw new SchemaError(`${what} must be text of 1 to ${MAX_TEXT} characters`);
  return v;
}

function bytes(v: Cbor, what: string, length?: number): Uint8Array {
  if (!(v instanceof Uint8Array) || (length !== undefined && v.length !== length)) {
    throw new SchemaError(`${what} must be a byte string${length !== undefined ? ` of ${length} bytes` : ""}`);
  }
  return v;
}

function array(v: Cbor, what: string, max: number): Cbor[] {
  if (!Array.isArray(v) || v.length > max) throw new SchemaError(`${what} must be an array of at most ${max}`);
  return v;
}

export function parseEnvelope(raw: Uint8Array): Envelope {
  const m = map(cbor(raw), "the envelope", ["body", "sig"]);
  return { body: bytes(m.body, "body"), sig: bytes(m.sig, "sig", 64) };
}

export function parseKeyList(body: Uint8Array): KeyList {
  const m = map(cbor(body), "the key list", ["tenant", "turn", "signer", "keys"]);
  const names = new Set<string>();
  const keys = array(m.keys, "keys", MAX_KEYS).map((k, i) => {
    const e = map(k, `keys[${i}]`, ["name", "key", "role", "not_before", "not_after"]);
    const key: Key = {
      name: textField(e.name, `keys[${i}].name`),
      key: bytes(e.key, `keys[${i}].key`, 32),
      role: textField(e.role, `keys[${i}].role`),
      not_before: uint(e.not_before, `keys[${i}].not_before`),
      not_after: uint(e.not_after, `keys[${i}].not_after`),
    };
    if (key.role !== "heads") throw new SchemaError(`keys[${i}].role must be "heads"`);
    if (key.not_after <= key.not_before) throw new SchemaError(`keys[${i}]: not_after must be after not_before`);
    if (names.has(key.name)) throw new SchemaError(`key ${key.name} is listed twice`);
    names.add(key.name);
    return key;
  });
  return { tenant: textField(m.tenant, "tenant"), turn: uint(m.turn, "turn"), signer: textField(m.signer, "signer"), keys };
}

export function parseHead(body: Uint8Array): Head {
  const m = map(cbor(body), "the head", ["tenant", "name", "turn", "created", "versions", "toolchain", "target", "segments", "signer"]);
  const v = m.versions;
  if (typeof v !== "object" || v === null || Array.isArray(v) || v instanceof Uint8Array) throw new SchemaError("versions must be a map");
  const versions: Record<string, number> = {};
  for (const [k, n] of Object.entries(v)) versions[textField(k, "a versions key")] = uint(n, `versions.${k}`);
  const seen = new Set<string>();
  const segments = array(m.segments, "segments", MAX_SEGMENTS).map((s, i) => {
    const b = bytes(s, `segments[${i}]`, 32);
    const h = hexOf(b);
    if (seen.has(h)) throw new SchemaError(`segment ${h} is named twice`);
    seen.add(h);
    return b;
  });
  return {
    tenant: textField(m.tenant, "tenant"),
    name: textField(m.name, "name"),
    turn: uint(m.turn, "turn"),
    created: uint(m.created, "created"),
    versions,
    toolchain: textField(m.toolchain, "toolchain"),
    target: textField(m.target, "target"),
    segments,
    signer: textField(m.signer, "signer"),
  };
}

/** A segment's entries, checked sorted and unique by (kind, key); `blobs` of every entry, in order. */
export function parseSegment(raw: Uint8Array): Entry[] {
  const m = map(cbor(raw), "the segment", ["v", "entries"]);
  if (m.v !== 1) throw new SchemaError("v must be 1");
  const enc = new TextEncoder();
  let prev: { kind: Uint8Array; key: Uint8Array } | null = null;
  return array(m.entries, "entries", Number.MAX_SAFE_INTEGER).map((e, i) => {
    if (!Array.isArray(e) || e.length !== 4) throw new SchemaError(`entries[${i}] must be [kind, key, blobs, data]`);
    const kind = textField(e[0], `entries[${i}].kind`);
    const key = bytes(e[1], `entries[${i}].key`);
    const blobs = array(e[2], `entries[${i}].blobs`, Number.MAX_SAFE_INTEGER).map((b, j) => {
      if (!Array.isArray(b) || b.length !== 2) throw new SchemaError(`entries[${i}].blobs[${j}] must be [sha256, ulen]`);
      return { hash: bytes(b[0], `entries[${i}].blobs[${j}] hash`, 32), ulen: uint(b[1], `entries[${i}].blobs[${j}] ulen`) };
    });
    const data = bytes(e[3], `entries[${i}].data`);
    const cur = { kind: enc.encode(kind), key };
    if (prev !== null) {
      const c = compareBytes(prev.kind, cur.kind) || compareBytes(prev.key, cur.key);
      if (c >= 0) throw new SchemaError(`entries[${i}] is not after entries[${i - 1}] in (kind, key) order`);
    }
    prev = cur;
    return { kind, key, blobs, data };
  });
}

export async function verify(publicKey: Uint8Array, context: Uint8Array, env: Envelope): Promise<boolean> {
  const key = await crypto.subtle.importKey("raw", publicKey, { name: "Ed25519" }, false, ["verify"]);
  const msg = new Uint8Array(context.length + env.body.length);
  msg.set(context, 0);
  msg.set(env.body, context.length);
  return crypto.subtle.verify({ name: "Ed25519" }, key, env.sig, msg);
}

function compareBytes(a: Uint8Array, b: Uint8Array): number {
  for (let i = 0; i < Math.min(a.length, b.length); i++) if (a[i] !== b[i]) return a[i] - b[i];
  return a.length - b.length;
}

function hexOf(b: Uint8Array): string {
  let s = "";
  for (const x of b) s += x.toString(16).padStart(2, "0");
  return s;
}
