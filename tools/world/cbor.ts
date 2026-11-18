// The world schema's deterministic CBOR (docs/v2/plan/design/world.md, "The encoding"), for
// exactly the subset it uses: unsigned integers up to 2^53 - 1, byte strings of 32 bytes, UTF-8
// text, arrays, maps keyed by small unsigned integers in ascending order, false and true. Every
// head is in its shortest form and every length definite; every length is checked against its
// limit and against the bytes left before anything is sized by it. No dependency.
//
// The same encoding as crates/blaze-world-api (enc.rs, dec.rs); tools/world/vectors.json holds
// the two to the same bytes.

import {
  CALLS,
  CHANGE_KINDS,
  EDIT_WHYS,
  ENTRY_KINDS,
  MOST_DEPTH,
  MOST_INT,
  REFUSAL_KINDS,
  REPLIES,
  SUBJECT_KINDS,
  TARGET_KINDS,
  VERSION,
  WHAT_KINDS,
  WIRE_CODES,
  isCheckout,
  isPath,
  type Change,
  type Edit,
  type Entered,
  type Entry,
  type Finding,
  type Limits,
  type Note,
  type Place,
  type Refusal,
  type Request,
  type Response,
  type Spot,
  type Subject,
  type Target,
  type What,
  type WireCode,
} from "./schema.ts";

/** Bytes refused: where, what was wrong, which field, and the number it is about. */
export class WireError extends Error {
  constructor(
    readonly at: number,
    readonly code: WireCode,
    readonly field: string,
    readonly said: number = 0,
  ) {
    super(`world schema ${VERSION}: byte ${at}, field \`${field}\`: ${code}${said ? ` (${said})` : ""}`);
    this.name = "WireError";
  }
}

/** A value this side was asked to encode that the schema has no bytes for. */
export class SchemaError extends Error {
  constructor(
    readonly field: string,
    readonly why: string,
  ) {
    super(`world schema ${VERSION}: field \`${field}\`: ${why}`);
    this.name = "SchemaError";
  }
}

const UINT = 0;
const BYTES = 2;
const TEXT = 3;
const ARRAY = 4;
const MAP = 5;
const FALSE = 0xf4;
const TRUE = 0xf5;
const MOST_FIELDS = 16;
const TWO_32 = 0x1_0000_0000;

const utf8 = new TextEncoder();
const strictUtf8 = new TextDecoder("utf-8", { fatal: true });

// ---------------------------------------------------------------------------------------------
// Writer.

class Writer {
  private buf = new Uint8Array(256);
  private len = 0;

  private room(n: number): void {
    if (this.len + n <= this.buf.length) return;
    let size = this.buf.length * 2;
    while (size < this.len + n) size *= 2;
    const next = new Uint8Array(size);
    next.set(this.buf.subarray(0, this.len));
    this.buf = next;
  }

  private byte(b: number): void {
    this.room(1);
    this.buf[this.len++] = b;
  }

  head(major: number, n: number): void {
    const m = major << 5;
    if (n < 24) {
      this.byte(m | n);
    } else if (n <= 0xff) {
      this.byte(m | 24);
      this.byte(n);
    } else if (n <= 0xffff) {
      this.byte(m | 25);
      this.byte(n >>> 8);
      this.byte(n & 0xff);
    } else if (n < TWO_32) {
      this.byte(m | 26);
      for (let shift = 24; shift >= 0; shift -= 8) this.byte((n >>> shift) & 0xff);
    } else {
      this.byte(m | 27);
      const high = Math.floor(n / TWO_32);
      const low = n % TWO_32;
      for (let shift = 24; shift >= 0; shift -= 8) this.byte((high >>> shift) & 0xff);
      for (let shift = 24; shift >= 0; shift -= 8) this.byte((low >>> shift) & 0xff);
    }
  }

  raw(bytes: Uint8Array): void {
    this.room(bytes.length);
    this.buf.set(bytes, this.len);
    this.len += bytes.length;
  }

  bytes(): Uint8Array {
    return this.buf.slice(0, this.len);
  }
}

/** The encoder of one message: keys and values of the schema's shapes. */
class Enc {
  readonly w = new Writer();

  map(entries: number): void {
    this.w.head(MAP, entries);
  }

  key(k: number): void {
    this.w.head(UINT, k);
  }

  uint(k: number, n: number, field: string): void {
    if (!Number.isSafeInteger(n) || n < 0) throw new SchemaError(field, `${n} is not an integer from 0 to 2^53 - 1`);
    this.key(k);
    this.w.head(UINT, n);
  }

  flag(k: number, value: boolean): void {
    this.key(k);
    this.w.raw(Uint8Array.of(value ? TRUE : FALSE));
  }

  id(k: number, hex: string, field: string): void {
    if (!/^[0-9a-f]{64}$/.test(hex)) throw new SchemaError(field, "an id is 64 lowercase hex digits");
    const out = new Uint8Array(32);
    for (let i = 0; i < 32; i++) out[i] = parseInt(hex.slice(2 * i, 2 * i + 2), 16);
    this.key(k);
    this.w.head(BYTES, 32);
    this.w.raw(out);
  }

  rawText(text: string): void {
    const bytes = utf8.encode(text);
    this.w.head(TEXT, bytes.length);
    this.w.raw(bytes);
  }

  text(k: number, text: string): void {
    this.key(k);
    this.rawText(text);
  }

  path(k: number, text: string, field: string, root = false): void {
    if (!(isPath(text) || (root && text === ""))) throw new SchemaError(field, `${JSON.stringify(text)} is not a world path`);
    this.text(k, text);
  }

  checkout(k: number, text: string, field: string): void {
    if (!isCheckout(text)) throw new SchemaError(field, `${JSON.stringify(text)} is not an absolute checkout`);
    this.text(k, text);
  }

  list<T>(k: number, items: readonly T[], each: (item: T) => void): void {
    this.key(k);
    this.w.head(ARRAY, items.length);
    for (const item of items) each(item);
  }

  tagged(entries: number, tag: number): void {
    this.map(entries);
    this.uint(0, tag, "kind");
  }

  code<T extends string>(k: number, table: readonly T[], value: T, field: string): void {
    const code = table.indexOf(value);
    if (code < 0) throw new SchemaError(field, `${JSON.stringify(value)} is not one of ${table.join(", ")}`);
    this.uint(k, code, field);
  }

  edit(e: Edit): void {
    switch (e.kind) {
      case "write_file":
        this.tagged(3, 0);
        this.path(1, e.path, "path");
        this.text(2, e.text);
        return;
      case "delete_file":
        this.tagged(2, 1);
        this.path(1, e.path, "path");
        return;
      case "rename_file":
        this.tagged(3, 2);
        this.path(1, e.from, "from");
        this.path(2, e.to, "to");
        return;
      case "replace_range":
        if (e.start > e.end) throw new SchemaError("end", "a range's start is past its end");
        this.tagged(6, 3);
        this.path(1, e.path, "path");
        this.uint(2, e.start, "start");
        this.uint(3, e.end, "end");
        this.text(4, e.text);
        this.text(5, e.expect);
        return;
      case "replace_item":
        this.tagged(3, 4);
        this.id(1, e.item, "item");
        this.text(2, e.text);
        return;
    }
  }

  what(x: What): void {
    this.tagged(2, WHAT_KINDS.indexOf(x.kind));
    switch (x.kind) {
      case "file":
        return this.path(1, x.path, "path");
      case "item":
        return this.id(1, x.key, "key");
      case "dir":
        return this.path(1, x.path, "path", true);
    }
  }

  target(t: Target): void {
    this.tagged(2, TARGET_KINDS.indexOf(t.kind));
    switch (t.kind) {
      case "path":
        return this.path(1, t.path, "path");
      case "item":
        return this.id(1, t.key, "key");
    }
  }

  subject(s: Subject): void {
    this.tagged(2, SUBJECT_KINDS.indexOf(s.kind));
    switch (s.kind) {
      case "finding":
        return this.id(1, s.id, "id");
      case "item":
        return this.id(1, s.key, "key");
    }
  }

  place(p: Place): void {
    if (p.start > p.end) throw new SchemaError("end", "a place's start is past its end");
    this.map(4);
    this.uint(0, p.start, "start");
    this.uint(1, p.end, "end");
    this.uint(2, p.line, "line");
    this.uint(3, p.column, "column");
  }

  spot(s: Spot): void {
    this.map(3);
    this.id(0, s.item, "item");
    this.path(1, s.path, "path");
    this.key(2);
    this.place(s.place);
  }

  note(n: Note): void {
    this.map(2);
    this.text(0, n.message);
    this.list(1, n.at, (s) => this.spot(s));
  }

  finding(f: Finding): void {
    this.map(6);
    this.id(0, f.id, "id");
    this.text(1, f.code);
    this.text(2, f.message);
    this.key(3);
    this.spot(f.at);
    this.list(4, f.notes, (n) => this.note(n));
    this.list(5, f.fix, (e) => this.edit(e));
  }

  entered(e: Entered): void {
    this.map(4);
    this.uint(0, e.names, "names");
    this.uint(1, e.heads, "heads");
    this.uint(2, e.bodies, "bodies");
    this.uint(3, e.lints, "lints");
  }

  entry(e: Entry): void {
    this.map(2);
    this.text(0, e.name);
    this.code(1, ENTRY_KINDS, e.kind, "kind");
  }

  change(c: Change): void {
    const tag = CHANGE_KINDS.indexOf(c.kind);
    switch (c.kind) {
      case "file_added":
      case "file_deleted":
      case "file_changed":
        this.tagged(2, tag);
        return this.path(1, c.path, "path");
      case "item_added":
      case "item_removed":
      case "item_changed":
        this.tagged(3, tag);
        this.id(1, c.item, "item");
        return this.path(2, c.path, "path");
    }
  }

  refusal(r: Refusal): void {
    const tag = REFUSAL_KINDS.indexOf(r.kind);
    switch (r.kind) {
      case "edit":
        this.tagged(3, tag);
        this.uint(1, r.index, "index");
        return this.code(2, EDIT_WHYS, r.why, "why");
      case "no_such_world":
        this.tagged(2, tag);
        return this.id(1, r.world, "world");
      case "not_a_checkout":
        this.tagged(2, tag);
        return this.checkout(1, r.checkout, "checkout");
      case "no_such_path":
        this.tagged(2, tag);
        return this.path(1, r.path, "path");
      case "no_such_item":
        this.tagged(2, tag);
        return this.id(1, r.item, "item");
      case "no_such_finding":
        this.tagged(2, tag);
        return this.id(1, r.id, "id");
      case "wire":
        this.tagged(5, tag);
        this.uint(1, r.at, "at");
        this.code(2, WIRE_CODES, r.what, "what");
        this.text(3, r.field);
        return this.uint(4, r.said, "said");
      case "io":
        this.tagged(3, tag);
        this.path(1, r.path, "path");
        return this.text(2, r.message);
      case "unsupported":
        this.tagged(3, tag);
        this.code(1, CALLS, r.call, "call");
        return this.text(2, r.why);
    }
  }
}

/** The request's bytes. */
export function encodeRequest(r: Request): Uint8Array {
  if (r.v !== VERSION) throw new SchemaError("v", `this side speaks version ${VERSION}`);
  const e = new Enc();
  const fields = { open: 1, patch: 2, read: 2, check: 3, diff: 2, commit: 3, explain: 2 }[r.call];
  e.map(2 + fields);
  e.uint(0, VERSION, "v");
  e.code(1, CALLS, r.call, "call");
  switch (r.call) {
    case "open":
      e.checkout(2, r.checkout, "checkout");
      break;
    case "patch":
      e.id(2, r.base, "base");
      e.list(3, r.edits, (x) => e.edit(x));
      break;
    case "read":
      e.id(2, r.world, "world");
      e.key(3);
      e.what(r.what);
      break;
    case "check":
      e.id(2, r.world, "world");
      e.list(3, r.select, (t) => e.target(t));
      e.flag(4, r.lints);
      break;
    case "diff":
      e.id(2, r.from, "from");
      e.id(3, r.to, "to");
      break;
    case "commit":
      e.id(2, r.world, "world");
      e.checkout(3, r.onto, "onto");
      e.id(4, r.expect, "expect");
      break;
    case "explain":
      e.id(2, r.world, "world");
      e.key(3);
      e.subject(r.subject);
      break;
  }
  return e.w.bytes();
}

/** The response's bytes. */
export function encodeResponse(r: Response): Uint8Array {
  if (r.v !== VERSION) throw new SchemaError("v", `this side speaks version ${VERSION}`);
  const e = new Enc();
  const fields = { world: 1, refused: 1, text: 3, listing: 1, checked: 5, changes: 1, committed: 1, moved: 1, explained: 2 }[
    r.reply
  ];
  e.map(2 + fields);
  e.uint(0, VERSION, "v");
  e.code(1, REPLIES, r.reply, "reply");
  switch (r.reply) {
    case "world":
      e.id(2, r.world, "world");
      break;
    case "refused":
      e.key(2);
      e.refusal(r.refusal);
      break;
    case "text":
      e.path(2, r.path, "path");
      e.uint(3, r.start, "start");
      e.text(4, r.text);
      break;
    case "listing":
      e.list(2, r.entries, (x) => e.entry(x));
      break;
    case "checked":
      e.id(2, r.world, "world");
      e.list(3, r.findings, (f) => e.finding(f));
      e.flag(4, r.certified);
      e.key(5);
      e.entered(r.entered);
      e.uint(6, r.ms, "ms");
      break;
    case "changes":
      e.list(2, r.changes, (c) => e.change(c));
      break;
    case "committed":
      e.uint(2, r.files_written, "files_written");
      break;
    case "moved":
      e.id(2, r.now, "now");
      break;
    case "explained":
      e.text(2, r.why);
      e.list(3, r.reads, (read) => e.rawText(read));
      break;
  }
  return e.w.bytes();
}

// ---------------------------------------------------------------------------------------------
// Reader.

type Fields = { left: number; last: number | null };

class Dec {
  at = 0;
  constructor(
    readonly bytes: Uint8Array,
    readonly limits: Limits,
  ) {
    if (bytes.length > limits.mostBytes) throw new WireError(0, "too_large", "message", limits.mostBytes);
  }

  private left(): number {
    return this.bytes.length - this.at;
  }

  /** A head of `major`, shortest form only; its number (lengths and integers past 2^53 - 1 refused). */
  private head(major: number, field: string): number {
    const start = this.at;
    if (start >= this.bytes.length) throw new WireError(start, "cbor", field);
    const first = this.bytes[start]!;
    if (first >> 5 !== major) throw new WireError(start, "cbor", field);
    const info = first & 0x1f;
    let extra: number;
    let least: number;
    if (info < 24) {
      this.at += 1;
      return info;
    } else if (info === 24) [extra, least] = [1, 24];
    else if (info === 25) [extra, least] = [2, 0x100];
    else if (info === 26) [extra, least] = [4, 0x1_0000];
    else if (info === 27) [extra, least] = [8, TWO_32];
    else throw new WireError(start, "indefinite", field);
    if (start + 1 + extra > this.bytes.length) throw new WireError(start, "cbor", field);
    let n = 0;
    for (let i = 0; i < extra; i++) {
      n = n * 256 + this.bytes[start + 1 + i]!;
      if (n > MOST_INT) throw new WireError(start, major === UINT ? "range" : "cbor", field, MOST_INT);
    }
    if (n < least) throw new WireError(start, "not_shortest", field);
    this.at = start + 1 + extra;
    return n;
  }

  rawUint(field: string): number {
    return this.head(UINT, field);
  }

  open(field: string): Fields {
    const start = this.at;
    const n = this.head(MAP, field);
    if (n > MOST_FIELDS || n > this.left()) throw new WireError(start, "cbor", field);
    return { left: n, last: null };
  }

  private order(f: Fields, k: number, at: number, field: string): void {
    if (f.last !== null && k === f.last) throw new WireError(at, "duplicate_key", field, k);
    if (f.last !== null && k < f.last) throw new WireError(at, "key_order", field, k);
  }

  key(f: Fields, want: number, field: string): void {
    const at = this.at;
    if (f.left === 0) throw new WireError(at, "missing_key", field, want);
    const k = this.rawUint(field);
    this.order(f, k, at, field);
    if (k > want) throw new WireError(at, "missing_key", field, want);
    if (k < want) throw new WireError(at, "unknown_key", field, k);
    f.left -= 1;
    f.last = k;
  }

  close(f: Fields, field: string): void {
    if (f.left === 0) return;
    const at = this.at;
    const k = this.rawUint(field);
    this.order(f, k, at, field);
    throw new WireError(at, "unknown_key", field, k);
  }

  uint(f: Fields, k: number, field: string): number {
    this.key(f, k, field);
    return this.rawUint(field);
  }

  flag(f: Fields, k: number, field: string): boolean {
    this.key(f, k, field);
    const at = this.at;
    const b = this.bytes[at];
    if (b === TRUE || b === FALSE) {
      this.at += 1;
      return b === TRUE;
    }
    throw new WireError(at, "cbor", field);
  }

  id(f: Fields, k: number, field: string): string {
    this.key(f, k, field);
    const start = this.at;
    const len = this.head(BYTES, field);
    if (len > 32) throw new WireError(start, "range", field);
    if (len > this.left()) throw new WireError(start, "cbor", field);
    if (len !== 32) throw new WireError(start, "range", field);
    let hex = "";
    for (let i = 0; i < 32; i++) hex += this.bytes[this.at + i]!.toString(16).padStart(2, "0");
    this.at += 32;
    return hex;
  }

  rawText(most: number, field: string): string {
    const start = this.at;
    const len = this.head(TEXT, field);
    if (len > most) throw new WireError(start, "too_long", field, most);
    if (len > this.left()) throw new WireError(start, "cbor", field);
    let text: string;
    try {
      text = strictUtf8.decode(this.bytes.subarray(this.at, this.at + len));
    } catch {
      throw new WireError(start, "cbor", field);
    }
    this.at += len;
    return text;
  }

  text(f: Fields, k: number, field: string): string {
    this.key(f, k, field);
    return this.rawText(this.limits.mostText, field);
  }

  name(f: Fields, k: number, field: string): string {
    this.key(f, k, field);
    return this.rawText(this.limits.mostPath, field);
  }

  path(f: Fields, k: number, field: string, root = false): string {
    this.key(f, k, field);
    const at = this.at;
    const text = this.rawText(this.limits.mostPath, field);
    if (!(isPath(text) || (root && text === ""))) throw new WireError(at, "bad_path", field);
    return text;
  }

  checkout(f: Fields, k: number, field: string): string {
    this.key(f, k, field);
    const at = this.at;
    const text = this.rawText(this.limits.mostPath, field);
    if (!isCheckout(text)) throw new WireError(at, "bad_path", field);
    return text;
  }

  list<T>(f: Fields, k: number, field: string, most: number, each: () => T): T[] {
    this.key(f, k, field);
    const start = this.at;
    const len = this.head(ARRAY, field);
    if (len > most) throw new WireError(start, "too_many", field, most);
    if (len > this.left()) throw new WireError(start, "cbor", field);
    const out: T[] = [];
    for (let i = 0; i < len; i++) out.push(each());
    return out;
  }

  code<T extends string>(f: Fields, k: number, field: string, table: readonly T[]): T {
    this.key(f, k, field);
    const at = this.at;
    const code = this.rawUint(field);
    const value = table[code];
    if (value === undefined) throw new WireError(at, "unknown_variant", field, code);
    return value;
  }

  /** A message's map, its version and its call or reply. */
  message(field: string): [Fields, number, number] {
    const f = this.open(field);
    this.key(f, 0, "v");
    let at = this.at;
    const version = this.rawUint("v");
    if (version !== VERSION) throw new WireError(at, "version", "v", version);
    this.key(f, 1, field);
    at = this.at;
    return [f, this.rawUint(field), at];
  }

  variant(field: string): [Fields, number, number] {
    const f = this.open(field);
    this.key(f, 0, field);
    const at = this.at;
    return [f, this.rawUint(field), at];
  }

  end(): void {
    if (this.at !== this.bytes.length) throw new WireError(this.at, "trailing", "message");
  }

  edit(): Edit {
    const [f, tag, at] = this.variant("edit");
    let e: Edit;
    switch (tag) {
      case 0:
        e = { kind: "write_file", path: this.path(f, 1, "path"), text: this.text(f, 2, "text") };
        break;
      case 1:
        e = { kind: "delete_file", path: this.path(f, 1, "path") };
        break;
      case 2:
        e = { kind: "rename_file", from: this.path(f, 1, "from"), to: this.path(f, 2, "to") };
        break;
      case 3: {
        const path = this.path(f, 1, "path");
        const start = this.uint(f, 2, "start");
        const endAt = this.at;
        const end = this.uint(f, 3, "end");
        if (start > end) throw new WireError(endAt, "range", "end", end);
        e = { kind: "replace_range", path, start, end, text: this.text(f, 4, "text"), expect: this.text(f, 5, "expect") };
        break;
      }
      case 4:
        e = { kind: "replace_item", item: this.id(f, 1, "item"), text: this.text(f, 2, "text") };
        break;
      default:
        throw new WireError(at, "unknown_variant", "edit", tag);
    }
    this.close(f, "edit");
    return e;
  }

  what(): What {
    const [f, tag, at] = this.variant("what");
    let w: What;
    if (tag === 0) w = { kind: "file", path: this.path(f, 1, "path") };
    else if (tag === 1) w = { kind: "item", key: this.id(f, 1, "key") };
    else if (tag === 2) w = { kind: "dir", path: this.path(f, 1, "path", true) };
    else throw new WireError(at, "unknown_variant", "what", tag);
    this.close(f, "what");
    return w;
  }

  target(): Target {
    const [f, tag, at] = this.variant("select");
    let t: Target;
    if (tag === 0) t = { kind: "path", path: this.path(f, 1, "path") };
    else if (tag === 1) t = { kind: "item", key: this.id(f, 1, "key") };
    else throw new WireError(at, "unknown_variant", "select", tag);
    this.close(f, "select");
    return t;
  }

  subject(): Subject {
    const [f, tag, at] = this.variant("subject");
    let s: Subject;
    if (tag === 0) s = { kind: "finding", id: this.id(f, 1, "id") };
    else if (tag === 1) s = { kind: "item", key: this.id(f, 1, "key") };
    else throw new WireError(at, "unknown_variant", "subject", tag);
    this.close(f, "subject");
    return s;
  }

  request(): Request {
    const [f, code, at] = this.message("call");
    const call = CALLS[code];
    if (call === undefined) throw new WireError(at, "unknown_variant", "call", code);
    let r: Request;
    switch (call) {
      case "open":
        r = { v: 1, call, checkout: this.checkout(f, 2, "checkout") };
        break;
      case "patch":
        r = { v: 1, call, base: this.id(f, 2, "base"), edits: this.list(f, 3, "edits", this.limits.mostEdits, () => this.edit()) };
        break;
      case "read": {
        const world = this.id(f, 2, "world");
        this.key(f, 3, "what");
        r = { v: 1, call, world, what: this.what() };
        break;
      }
      case "check":
        r = {
          v: 1,
          call,
          world: this.id(f, 2, "world"),
          select: this.list(f, 3, "select", this.limits.mostList, () => this.target()),
          lints: this.flag(f, 4, "lints"),
        };
        break;
      case "diff":
        r = { v: 1, call, from: this.id(f, 2, "from"), to: this.id(f, 3, "to") };
        break;
      case "commit":
        r = { v: 1, call, world: this.id(f, 2, "world"), onto: this.checkout(f, 3, "onto"), expect: this.id(f, 4, "expect") };
        break;
      case "explain": {
        const world = this.id(f, 2, "world");
        this.key(f, 3, "subject");
        r = { v: 1, call, world, subject: this.subject() };
        break;
      }
    }
    this.close(f, "request");
    return r;
  }

  place(): Place {
    const f = this.open("place");
    const start = this.uint(f, 0, "start");
    const endAt = this.at;
    const end = this.uint(f, 1, "end");
    if (start > end) throw new WireError(endAt, "range", "end", end);
    const p: Place = { start, end, line: this.uint(f, 2, "line"), column: this.uint(f, 3, "column") };
    this.close(f, "place");
    return p;
  }

  spot(): Spot {
    const f = this.open("spot");
    const item = this.id(f, 0, "item");
    const path = this.path(f, 1, "path");
    this.key(f, 2, "place");
    const s: Spot = { item, path, place: this.place() };
    this.close(f, "spot");
    return s;
  }

  note(): Note {
    const f = this.open("note");
    const n: Note = { message: this.text(f, 0, "message"), at: this.list(f, 1, "at", this.limits.mostList, () => this.spot()) };
    this.close(f, "note");
    return n;
  }

  finding(): Finding {
    const f = this.open("finding");
    const id = this.id(f, 0, "id");
    const code = this.name(f, 1, "code");
    const message = this.text(f, 2, "message");
    this.key(f, 3, "at");
    const at = this.spot();
    const notes = this.list(f, 4, "notes", this.limits.mostList, () => this.note());
    const fix = this.list(f, 5, "fix", this.limits.mostEdits, () => this.edit());
    this.close(f, "finding");
    return { id, code, message, at, notes, fix };
  }

  entered(): Entered {
    const f = this.open("entered");
    const e: Entered = {
      names: this.uint(f, 0, "names"),
      heads: this.uint(f, 1, "heads"),
      bodies: this.uint(f, 2, "bodies"),
      lints: this.uint(f, 3, "lints"),
    };
    this.close(f, "entered");
    return e;
  }

  entry(): Entry {
    const f = this.open("entry");
    const e: Entry = { name: this.name(f, 0, "name"), kind: this.code(f, 1, "kind", ENTRY_KINDS) };
    this.close(f, "entry");
    return e;
  }

  change(): Change {
    const [f, tag, at] = this.variant("change");
    let c: Change;
    if (tag === 0) c = { kind: "file_added", path: this.path(f, 1, "path") };
    else if (tag === 1) c = { kind: "file_deleted", path: this.path(f, 1, "path") };
    else if (tag === 2) c = { kind: "file_changed", path: this.path(f, 1, "path") };
    else if (tag === 3) c = { kind: "item_added", item: this.id(f, 1, "item"), path: this.path(f, 2, "path") };
    else if (tag === 4) c = { kind: "item_removed", item: this.id(f, 1, "item"), path: this.path(f, 2, "path") };
    else if (tag === 5) c = { kind: "item_changed", item: this.id(f, 1, "item"), path: this.path(f, 2, "path") };
    else throw new WireError(at, "unknown_variant", "change", tag);
    this.close(f, "change");
    return c;
  }

  refusal(): Refusal {
    const [f, tag, at] = this.variant("refusal");
    let r: Refusal;
    switch (tag) {
      case 0:
        r = { kind: "edit", index: this.uint(f, 1, "index"), why: this.code(f, 2, "why", EDIT_WHYS) };
        break;
      case 1:
        r = { kind: "no_such_world", world: this.id(f, 1, "world") };
        break;
      case 2:
        r = { kind: "not_a_checkout", checkout: this.checkout(f, 1, "checkout") };
        break;
      case 3:
        r = { kind: "no_such_path", path: this.path(f, 1, "path") };
        break;
      case 4:
        r = { kind: "no_such_item", item: this.id(f, 1, "item") };
        break;
      case 5:
        r = { kind: "no_such_finding", id: this.id(f, 1, "id") };
        break;
      case 6:
        r = {
          kind: "wire",
          at: this.uint(f, 1, "at"),
          what: this.code(f, 2, "what", WIRE_CODES),
          field: this.name(f, 3, "field"),
          said: this.uint(f, 4, "said"),
        };
        break;
      case 7:
        r = { kind: "io", path: this.path(f, 1, "path"), message: this.text(f, 2, "message") };
        break;
      case 8:
        r = { kind: "unsupported", call: this.code(f, 1, "call", CALLS), why: this.text(f, 2, "why") };
        break;
      default:
        throw new WireError(at, "unknown_variant", "refusal", tag);
    }
    this.close(f, "refusal");
    return r;
  }

  response(): Response {
    const [f, code, at] = this.message("reply");
    const reply = REPLIES[code];
    if (reply === undefined) throw new WireError(at, "unknown_variant", "reply", code);
    let r: Response;
    switch (reply) {
      case "world":
        r = { v: 1, reply, world: this.id(f, 2, "world") };
        break;
      case "refused":
        this.key(f, 2, "refusal");
        r = { v: 1, reply, refusal: this.refusal() };
        break;
      case "text":
        r = { v: 1, reply, path: this.path(f, 2, "path"), start: this.uint(f, 3, "start"), text: this.text(f, 4, "text") };
        break;
      case "listing":
        r = { v: 1, reply, entries: this.list(f, 2, "entries", this.limits.mostList, () => this.entry()) };
        break;
      case "checked": {
        const world = this.id(f, 2, "world");
        const findings = this.list(f, 3, "findings", this.limits.mostList, () => this.finding());
        const certified = this.flag(f, 4, "certified");
        this.key(f, 5, "entered");
        const entered = this.entered();
        r = { v: 1, reply, world, findings, certified, entered, ms: this.uint(f, 6, "ms") };
        break;
      }
      case "changes":
        r = { v: 1, reply, changes: this.list(f, 2, "changes", this.limits.mostList, () => this.change()) };
        break;
      case "committed":
        r = { v: 1, reply, files_written: this.uint(f, 2, "files_written") };
        break;
      case "moved":
        r = { v: 1, reply, now: this.id(f, 2, "now") };
        break;
      case "explained":
        r = {
          v: 1,
          reply,
          why: this.text(f, 2, "why"),
          reads: this.list(f, 3, "reads", this.limits.mostList, () => this.rawText(this.limits.mostPath, "reads")),
        };
        break;
    }
    this.close(f, "response");
    return r;
  }
}

/** The request `bytes` hold, and nothing after it. */
export function decodeRequest(bytes: Uint8Array, limits: Limits): Request {
  const d = new Dec(bytes, limits);
  const r = d.request();
  d.end();
  return r;
}

/** The response `bytes` hold, and nothing after it. */
export function decodeResponse(bytes: Uint8Array, limits: Limits): Response {
  const d = new Dec(bytes, limits);
  const r = d.response();
  d.end();
  return r;
}

/**
 * Where the first CBOR item of `bytes` ends, or `null` while it is not whole: what a reader of a
 * connection waits on, as crates/blaze-world-api's `Session` scans. Indefinite lengths, tags,
 * floats and simple values other than false and true are refused here, before any decode.
 */
export function itemEnd(bytes: Uint8Array, limits: Limits): number | null {
  const left: number[] = [1];
  let at = 0;
  while (left.length > 0) {
    if (at >= bytes.length) return null;
    const first = bytes[at]!;
    const major = first >> 5;
    const info = first & 0x1f;
    let len = 1;
    let opens = 0;
    if (major === 7) {
      if (first !== FALSE && first !== TRUE) throw new WireError(at, "cbor", "message");
    } else if (major === 6 || major === 1) {
      throw new WireError(at, "cbor", "message");
    } else {
      const extra = info < 24 ? 0 : info === 24 ? 1 : info === 25 ? 2 : info === 26 ? 4 : info === 27 ? 8 : -1;
      if (extra < 0) throw new WireError(at, "indefinite", "message");
      if (at + 1 + extra > bytes.length) return null;
      let n = info < 24 ? info : 0;
      for (let i = 0; i < extra; i++) n = n * 256 + bytes[at + 1 + i]!;
      len = 1 + extra;
      if (major === BYTES || major === TEXT) {
        if (at + len + n > limits.mostBytes) throw new WireError(at, "too_large", "message", limits.mostBytes);
        if (at + len + n > bytes.length) return null;
        len += n;
      } else if (major === ARRAY) {
        opens = n;
      } else if (major === MAP) {
        opens = 2 * n;
      }
    }
    at += len;
    left[left.length - 1]! -= 1;
    if (opens > 0) {
      if (left.length === MOST_DEPTH + 1) throw new WireError(at - len, "too_deep", "message", MOST_DEPTH);
      left.push(opens);
    }
    while (left.length > 0 && left[left.length - 1] === 0) left.pop();
  }
  return at;
}
