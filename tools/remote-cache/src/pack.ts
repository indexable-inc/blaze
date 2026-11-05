// The pack stream (API.md, "Pack stream") and the stored `.bpk` (API.md, "Storage").

import { encode } from "./cbor";

export const STREAM_MAGIC = new TextEncoder().encode("BLZSTRM1");
export const PACK_MAGIC = new TextEncoder().encode("BLZPACK1");
export const MAX_ULEN = 4 * 1024 * 1024;

export const TAG_END = 0x00;
export const TAG_BLOB = 0x01;
export const TAG_MISSING = 0x02;

export const CODEC_ZSTD = 0;
export const CODEC_RAW = 1;
const FLAG_DICT = 0b100;

export interface Frame {
  hash: Uint8Array;
  codec: number;
  dict: Uint8Array | null;
  ulen: number;
  /** The frame's bytes, a view into the stream. */
  data: Uint8Array;
}

export class StreamError extends Error {}

/** Parses a whole pack stream as `/put` receives it. Throws `StreamError` when it is malformed or cut short. */
export function parseStream(bytes: Uint8Array): Frame[] {
  if (bytes.length < STREAM_MAGIC.length + 1 || !STREAM_MAGIC.every((b, i) => bytes[i] === b)) {
    throw new StreamError("not a pack stream (magic)");
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const frames: Frame[] = [];
  let pos = STREAM_MAGIC.length;
  const need = (n: number) => {
    if (pos + n > bytes.length) throw new StreamError(`cut short at byte ${pos}`);
  };
  for (;;) {
    need(1);
    const tag = bytes[pos++];
    if (tag === TAG_END) break;
    if (tag !== TAG_BLOB) throw new StreamError(`record tag ${tag} at byte ${pos - 1}`);
    need(33);
    const hash = bytes.subarray(pos, pos + 32);
    const flags = bytes[pos + 32];
    pos += 33;
    if (flags & ~0b111 || (flags & 0b11) > CODEC_RAW) throw new StreamError(`flags ${flags} at byte ${pos - 1}`);
    let dict: Uint8Array | null = null;
    if (flags & FLAG_DICT) {
      need(32);
      dict = bytes.subarray(pos, pos + 32);
      pos += 32;
    }
    need(8);
    const ulen = view.getUint32(pos, true);
    const clen = view.getUint32(pos + 4, true);
    pos += 8;
    need(clen);
    frames.push({ hash, codec: flags & 0b11, dict, ulen, data: bytes.subarray(pos, pos + clen) });
    pos += clen;
  }
  if (pos !== bytes.length) throw new StreamError("bytes after the end record");
  if (frames.length === 0) throw new StreamError("no blobs");
  return frames;
}

export function blobRecordHeader(hash: Uint8Array, codec: number, dict: Uint8Array | null, ulen: number, clen: number): Uint8Array {
  const out = new Uint8Array(1 + 33 + (dict ? 32 : 0) + 8);
  out[0] = TAG_BLOB;
  out.set(hash, 1);
  out[33] = codec | (dict ? FLAG_DICT : 0);
  let pos = 34;
  if (dict) {
    out.set(dict, pos);
    pos += 32;
  }
  const view = new DataView(out.buffer);
  view.setUint32(pos, ulen, true);
  view.setUint32(pos + 4, clen, true);
  return out;
}

export function missingRecord(hash: Uint8Array): Uint8Array {
  const out = new Uint8Array(33);
  out[0] = TAG_MISSING;
  out.set(hash, 1);
  return out;
}

export interface PackEntry {
  /** The namespace's key for the blob: its hash, or the tenant's HMAC of it. */
  key: Uint8Array;
  offset: number;
  clen: number;
  ulen: number;
  codec: number;
}

/** Lays frames out as a `.bpk`: header, frames, CBOR index, footer. */
export function buildPack(frames: Frame[], keys: Uint8Array[]): { bytes: Uint8Array; entries: PackEntry[] } {
  const entries: PackEntry[] = [];
  let offset = PACK_MAGIC.length;
  for (const [i, f] of frames.entries()) {
    entries.push({ key: keys[i], offset, clen: f.data.length, ulen: f.ulen, codec: f.codec });
    offset += f.data.length;
  }
  const index = encode(entries.map((e) => [e.key, e.offset, e.clen, e.ulen, e.codec]));
  const bytes = new Uint8Array(offset + index.length + 24);
  bytes.set(PACK_MAGIC, 0);
  for (const [i, f] of frames.entries()) bytes.set(f.data, entries[i].offset);
  bytes.set(index, offset);
  const footer = new DataView(bytes.buffer, offset + index.length, 24);
  footer.setBigUint64(0, BigInt(offset), true);
  footer.setBigUint64(8, BigInt(index.length), true);
  bytes.set(PACK_MAGIC, offset + index.length + 16);
  return { bytes, entries };
}
