// The TypeScript side of the world schema, against the golden vectors crates/blaze-world-api
// tests too (tools/world/vectors.json): every vector decodes to its JSON and encodes to its bytes;
// the encoding rules and limits are refused by name; and client.ts, over a Unix socket and over
// HTTP, against an in-process fake server that answers each request's exact bytes from the vectors.
//
//   bun test tools/world/client.test.ts

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { WireError, decodeRequest, decodeResponse, encodeRequest, encodeResponse, itemEnd } from "./cbor.ts";
import {
  Client,
  EditRefusedError,
  MovedError,
  RefusedError,
  TimeoutError,
  UnexpectedReplyError,
  connect,
  transaction,
} from "./client.ts";
import { DEFAULT_LIMITS, type Limits } from "./schema.ts";

type Vector = { name: string; json: Record<string, unknown>; bytes: Uint8Array };

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function fromHex(text: string): Uint8Array {
  const digits = text.replace(/\s+/g, "");
  if (!/^([0-9a-f]{2})*$/.test(digits)) throw new Error(`not hex: ${text}`);
  const out = new Uint8Array(digits.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(digits.slice(2 * i, 2 * i + 2), 16);
  return out;
}

function toHex(bytes: Uint8Array): string {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

const raw: unknown = JSON.parse(await Bun.file(new URL("./vectors.json", import.meta.url)).text());
if (!Array.isArray(raw)) throw new Error("tools/world/vectors.json is not an array");
const VECTORS: Vector[] = raw.map((entry: unknown) => {
  if (!isRecord(entry) || typeof entry.name !== "string" || !isRecord(entry.json) || typeof entry.cbor_hex !== "string")
    throw new Error(`a vector without name, json and cbor_hex: ${JSON.stringify(entry)}`);
  return { name: entry.name, json: entry.json, bytes: fromHex(entry.cbor_hex) };
});

function vector(name: string): Vector {
  const found = VECTORS.find((v) => v.name === name);
  if (found === undefined) throw new Error(`no vector named ${name}`);
  return found;
}

const A = "aa".repeat(32);
const B = "bb".repeat(32);
const C = "cc".repeat(32);
const D = "dd".repeat(32);

function refusedCode(f: () => unknown): string {
  try {
    f();
  } catch (error) {
    if (error instanceof WireError) return `${error.code} ${error.field} ${error.said}`;
    throw error;
  }
  throw new Error("not refused");
}

describe("the golden vectors", () => {
  for (const v of VECTORS) {
    test(v.name, () => {
      if ("call" in v.json) {
        const request = decodeRequest(v.bytes, DEFAULT_LIMITS);
        expect(request as unknown).toEqual(v.json);
        expect(toHex(encodeRequest(request))).toBe(toHex(v.bytes));
      } else {
        const response = decodeResponse(v.bytes, DEFAULT_LIMITS);
        expect(response as unknown).toEqual(v.json);
        expect(toHex(encodeResponse(response))).toBe(toHex(v.bytes));
      }
      // A reader of a connection finds the item's end only once it is all there.
      for (let cut = 0; cut < v.bytes.length; cut++) expect(itemEnd(v.bytes.subarray(0, cut), DEFAULT_LIMITS)).toBeNull();
      expect(itemEnd(v.bytes, DEFAULT_LIMITS)).toBe(v.bytes.length);
    });
  }

  test("every call, reply and refusal has a vector", () => {
    const calls = new Set(VECTORS.map((v) => v.json.call).filter((c) => c !== undefined));
    const replies = new Set(VECTORS.map((v) => v.json.reply).filter((r) => r !== undefined));
    const refusals = new Set(VECTORS.map((v) => (isRecord(v.json.refusal) ? v.json.refusal.kind : undefined)).filter((k) => k !== undefined));
    expect(calls.size).toBe(7);
    expect(replies.size).toBe(9);
    expect(refusals.size).toBe(9);
  });
});

describe("refused by name", () => {
  const open = vector("request open").bytes; // a3 00 01 01 00 02 62 2f 77
  const at = (bytes: number[]): Uint8Array => Uint8Array.from(bytes);

  test("an unknown version", () => {
    expect(refusedCode(() => decodeRequest(at([0xa3, 0x00, 0x02, 0x01, 0x00, 0x02, 0x62, 0x2f, 0x77]), DEFAULT_LIMITS))).toBe("version v 2");
  });
  test("an unknown key", () => {
    expect(refusedCode(() => decodeRequest(at([0xa4, ...open.slice(1), 0x09, 0x00]), DEFAULT_LIMITS))).toBe(
      "unknown_key request 9",
    );
  });
  test("a duplicate key", () => {
    expect(refusedCode(() => decodeRequest(at([0xa3, 0x00, 0x01, 0x00, 0x01, 0x02, 0x62, 0x2f, 0x77]), DEFAULT_LIMITS))).toBe(
      "duplicate_key call 0",
    );
  });
  test("a missing key", () => {
    expect(refusedCode(() => decodeRequest(at([0xa2, 0x00, 0x01, 0x01, 0x00]), DEFAULT_LIMITS))).toBe("missing_key checkout 2");
  });
  test("a non-shortest integer and length", () => {
    expect(refusedCode(() => decodeRequest(at([0xa3, 0x00, 0x18, 0x01, 0x01, 0x00, 0x02, 0x62, 0x2f, 0x77]), DEFAULT_LIMITS))).toBe(
      "not_shortest v 0",
    );
    expect(refusedCode(() => decodeRequest(at([0xa3, 0x00, 0x01, 0x01, 0x00, 0x02, 0x78, 0x02, 0x2f, 0x77]), DEFAULT_LIMITS))).toBe(
      "not_shortest checkout 0",
    );
  });
  test("an indefinite length", () => {
    expect(refusedCode(() => decodeRequest(at([0xbf, 0x00, 0x01, 0xff]), DEFAULT_LIMITS))).toBe("indefinite call 0");
    expect(refusedCode(() => itemEnd(at([0x7f, 0x61, 0x61, 0xff]), DEFAULT_LIMITS))).toBe("indefinite message 0");
  });
  test("an unknown call", () => {
    expect(refusedCode(() => decodeRequest(at([0xa3, 0x00, 0x01, 0x01, 0x07, 0x02, 0x62, 0x2f, 0x77]), DEFAULT_LIMITS))).toBe(
      "unknown_variant call 7",
    );
  });
  test("a relative checkout", () => {
    expect(refusedCode(() => decodeRequest(at([0xa3, 0x00, 0x01, 0x01, 0x00, 0x02, 0x61, 0x77]), DEFAULT_LIMITS))).toBe("bad_path checkout 0");
  });
  test("an integer past 2^53 - 1", () => {
    const big = [0x1b, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    expect(refusedCode(() => decodeResponse(at([0xa3, 0x00, 0x01, 0x01, 0x06, 0x02, ...big]), DEFAULT_LIMITS))).toBe(
      `range files_written ${Number.MAX_SAFE_INTEGER}`,
    );
  });
  test("bytes after the message", () => {
    expect(refusedCode(() => decodeRequest(at([...open, 0x00]), DEFAULT_LIMITS))).toBe("trailing message 0");
  });
  test("nesting past the schema's depth", () => {
    expect(refusedCode(() => itemEnd(new Uint8Array(12).fill(0x81), DEFAULT_LIMITS))).toBe("too_deep message 8");
  });

  test("every limit, one over", () => {
    const tight: Limits = { mostBytes: open.length, mostText: 4, mostPath: 2, mostEdits: 1, mostList: 1 };
    expect(decodeRequest(open, tight)).toEqual({ v: 1, call: "open", checkout: "/w" });
    expect(refusedCode(() => decodeRequest(open, { ...tight, mostBytes: open.length - 1 }))).toBe(`too_large message ${open.length - 1}`);
    expect(refusedCode(() => decodeRequest(open, { ...tight, mostPath: 1 }))).toBe("too_long checkout 1");
    const one = vector("request patch: one body").bytes;
    expect(refusedCode(() => decodeRequest(one, { ...DEFAULT_LIMITS, mostEdits: 0 }))).toBe("too_many edits 0");
    expect(refusedCode(() => decodeRequest(one, { ...DEFAULT_LIMITS, mostText: 8 }))).toBe("too_long text 8");
    const selected = vector("request check: selected, with lints").bytes;
    // The same pair as the Rust test: a check exactly at its list limit and its own size is
    // accepted (another request's size is not its limit); one under either is refused.
    const atList: Limits = { ...DEFAULT_LIMITS, mostBytes: selected.length, mostList: 2 };
    expect(decodeRequest(selected, atList).call).toBe("check");
    expect(refusedCode(() => decodeRequest(selected, { ...atList, mostList: 1 }))).toBe("too_many select 1");
    expect(refusedCode(() => decodeRequest(selected, { ...atList, mostBytes: selected.length - 1 }))).toBe(
      `too_large message ${selected.length - 1}`,
    );    expect(refusedCode(() => itemEnd(at([0xa3, 0x00, 0x01, 0x01, 0x00, 0x02, 0x79, 0x03, 0xe8]), { ...DEFAULT_LIMITS, mostBytes: 64 }))).toBe(
      "too_large message 64",
    );
  });
});

// ---------------------------------------------------------------------------------------------
// The fake server: request bytes (hex) to response bytes, both taken from the vectors.

const answers = new Map<string, Uint8Array>();
const unanswered: string[] = [];

function answer(requestName: string, responseName: string): void {
  answers.set(toHex(vector(requestName).bytes), vector(responseName).bytes);
}

function reply(request: Uint8Array): Uint8Array {
  const found = answers.get(toHex(request));
  if (found !== undefined) return found;
  unanswered.push(toHex(request));
  return vector("refused: unsupported").bytes;
}

let dir = "";
let socketPath = "";
let socketServer: { stop(closeActiveConnections?: boolean): void } | null = null;
let httpServer: { stop(closeActiveConnections?: boolean): void; port: number | undefined } | null = null;

beforeAll(() => {
  dir = mkdtempSync(join(tmpdir(), "world-test-"));
  socketPath = join(dir, "world.sock");
  const received = new Map<object, Uint8Array>();
  socketServer = Bun.listen({
    unix: socketPath,
    socket: {
      data(socket, chunk) {
        const before = received.get(socket) ?? new Uint8Array(0);
        const got = new Uint8Array(before.length + chunk.length);
        got.set(before);
        got.set(chunk, before.length);
        received.set(socket, got);
        if (itemEnd(got, DEFAULT_LIMITS) === got.length) {
          socket.write(reply(got));
          socket.end();
        }
      },
    },
  });
  httpServer = Bun.serve({
    port: 0,
    async fetch(request) {
      const body = new Uint8Array(await request.arrayBuffer());
      return new Response(reply(body), { headers: { "content-type": "application/cbor" } });
    },
  });
});

afterAll(() => {
  socketServer?.stop(true);
  httpServer?.stop(true);
  rmSync(dir, { recursive: true, force: true });
});

function clients(): [string, Client][] {
  return [
    ["socket", connect({ socket: socketPath })],
    ["http", connect({ url: `http://127.0.0.1:${httpServer?.port}/` })],
  ];
}

describe("client.ts against the fake server", () => {
  test("every call, over a socket and over HTTP", async () => {
    answer("request open", "response world");
    answer("request patch: every edit", "response world");
    answer("request read: a file", "response text");
    answer("request read: the root directory", "response listing");
    answer("request check: the whole world", "response checked: clean");
    answer("request check: selected, with lints", "response checked: one finding with a fix");
    answer("request diff", "response changes: every change");
    answer("request commit", "response committed");
    answer("request explain: a finding", "response explained");
    for (const [over, client] of clients()) {
      const opened = await client.open("/w");
      expect([over, opened.id]).toEqual([over, B]);
      const a = client.world(A);
      const patched = await transaction(a, (tx) =>
        tx
          .writeFile("a.rs", "x")
          .deleteFile("b.rs")
          .renameFile("c.rs", "d.rs")
          .replaceRange("e.rs", 2, 5, "yz", "abc")
          .replaceItem(C, "fn f() {}"),
      );
      expect(patched.id).toBe(B);
      expect(await a.read({ kind: "file", path: "src/lib.rs" })).toEqual({ kind: "text", path: "src/lib.rs", start: 0, text: "fn f() {}" });
      expect(await a.read({ kind: "dir", path: "" })).toEqual({
        kind: "listing",
        entries: [
          { name: "lib.rs", kind: "file" },
          { name: "bin", kind: "dir" },
        ],
      });
      const clean = await a.check();
      expect([clean.certified, clean.findings.length]).toEqual([true, 0]);
      const found = await a.check({ select: [{ kind: "path", path: "src" }, { kind: "item", key: C }], lints: true });
      expect(found.findings[0]?.code).toBe("E0308");
      expect((await a.diff(B)).map((c) => c.kind)).toEqual([
        "file_added",
        "file_deleted",
        "file_changed",
        "item_added",
        "item_removed",
        "item_changed",
      ]);
      expect(await client.world(B).commit({ onto: "/w", expect: a })).toBe(2);
      expect(await client.world(B).explain({ kind: "finding", id: D })).toEqual({ why: "the head of f moved", reads: ["f", "g"] });
    }
    expect(unanswered).toEqual([]);
  });

  test("a moved checkout, a refused edit, a refusal and a wrong reply are typed errors", async () => {
    answer("request commit", "response moved");
    answer("request patch: one body", "refused: edit");
    answer("request read: an item", "refused: no such item");
    answer("request diff", "response text");
    for (const [, client] of clients()) {
      const a = client.world(A);
      const moved = await client.world(B).commit({ onto: "/w", expect: A }).catch((error: unknown) => error);
      expect(moved).toBeInstanceOf(MovedError);
      if (moved instanceof MovedError) expect([moved.expected, moved.now]).toEqual([A, B]);
      const edit = await a.patch([{ kind: "replace_item", item: C, text: "fn f() {}" }]).catch((error: unknown) => error);
      expect(edit).toBeInstanceOf(EditRefusedError);
      if (edit instanceof EditRefusedError) expect([edit.index, edit.why]).toEqual([3, "expect_mismatch"]);
      const item = await a.read({ kind: "item", key: C }).catch((error: unknown) => error);
      expect(item).toBeInstanceOf(RefusedError);
      if (item instanceof RefusedError) expect(item.refusal.kind).toBe("no_such_item");
      const wrong = await a.diff(B).catch((error: unknown) => error);
      expect(wrong).toBeInstanceOf(UnexpectedReplyError);
    }
    expect(unanswered).toEqual([]);
  });

  test("a call with no answer ends at its deadline", async () => {
    const silent = Bun.listen({ unix: join(dir, "silent.sock"), socket: { data() {} } });
    try {
      const client = connect({ socket: join(dir, "silent.sock") }, { timeoutMs: 50, limits: DEFAULT_LIMITS });
      const error = await client.open("/w").catch((e: unknown) => e);
      expect(error).toBeInstanceOf(TimeoutError);
    } finally {
      silent.stop(true);
    }
  });
});
