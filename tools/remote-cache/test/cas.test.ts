import * as zlib from "node:zlib";
import { describe, expect, it } from "vitest";
import { hex, MAX_PUT } from "../src/cas";
import { handle } from "../src/index";
import { CborError, decode, encode, type Cbor } from "../src/cbor";
import { blobRecordHeader, CODEC_RAW, CODEC_ZSTD, STREAM_MAGIC, TAG_BLOB, TAG_END, TAG_MISSING } from "../src/pack";
import { BASE, call, testEnv, TOKENS, type Opts, type TokenName } from "./fixture";

const enc = (s: string) => new TextEncoder().encode(s);

async function sha(bytes: Uint8Array): Promise<Uint8Array> {
  return new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
}

interface Blob {
  content: Uint8Array;
  codec?: number;
  /** Overrides, to build refused frames. */
  hash?: Uint8Array;
  ulen?: number;
  data?: Uint8Array;
  dict?: Uint8Array;
}

async function stream(blobs: Blob[], { end = true } = {}): Promise<Uint8Array> {
  const parts: Uint8Array[] = [STREAM_MAGIC];
  for (const b of blobs) {
    const codec = b.codec ?? CODEC_ZSTD;
    const data = b.data ?? (codec === CODEC_ZSTD ? zlib.zstdCompressSync(b.content) : b.content);
    parts.push(blobRecordHeader(b.hash ?? (await sha(b.content)), codec, b.dict ?? null, b.ulen ?? b.content.length, data.length), data);
  }
  if (end) parts.push(new Uint8Array([TAG_END]));
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let pos = 0;
  for (const p of parts) {
    out.set(p, pos);
    pos += p.length;
  }
  return out;
}

function put(token: TokenName | undefined, body: Uint8Array): Promise<Response> {
  return call("POST", "/put", { token, body, headers: { "content-length": String(body.length) } });
}

async function putOk(token: TokenName, blobs: Blob[]): Promise<{ pack: string; blobs: number }> {
  const r = await put(token, await stream(blobs));
  expect(r.status, r.status === 200 ? "" : await r.clone().text()).toBe(200);
  return decode(new Uint8Array(await r.arrayBuffer())) as { pack: string; blobs: number };
}

async function hasState(token: TokenName | undefined, hashes: Uint8Array[]): Promise<number[]> {
  const r = await call("POST", "/has", { token, body: encode({ hashes }) });
  expect(r.status).toBe(200);
  return (decode(new Uint8Array(await r.arrayBuffer())) as { state: number[] }).state;
}

interface Got {
  blobs: { hash: string; codec: number; ulen: number; data: Uint8Array }[];
  missing: string[];
}

async function getBlobs(token: TokenName | undefined, hashes: Uint8Array[]): Promise<Got> {
  const r = await call("POST", "/get", { token, body: encode({ hashes }) });
  expect(r.status).toBe(200);
  const b = new Uint8Array(await r.arrayBuffer());
  expect([...b.subarray(0, 8)]).toEqual([...STREAM_MAGIC]);
  const view = new DataView(b.buffer, b.byteOffset, b.byteLength);
  const got: Got = { blobs: [], missing: [] };
  let pos = 8;
  for (;;) {
    const tag = b[pos++];
    if (tag === TAG_END) break;
    if (tag === TAG_MISSING) {
      got.missing.push(hex(b.subarray(pos, pos + 32)));
      pos += 32;
      continue;
    }
    expect(tag).toBe(TAG_BLOB);
    const hash = hex(b.subarray(pos, pos + 32));
    const flags = b[pos + 32];
    pos += 33;
    const ulen = view.getUint32(pos, true);
    const clen = view.getUint32(pos + 4, true);
    pos += 8;
    got.blobs.push({ hash, codec: flags & 3, ulen, data: b.slice(pos, pos + clen) });
    pos += clen;
  }
  expect(pos).toBe(b.length);
  return got;
}

async function content(got: Got["blobs"][number]): Promise<Uint8Array> {
  return got.codec === CODEC_RAW ? got.data : zlib.zstdDecompressSync(got.data);
}

async function shape(r: Response) {
  const headers = [...r.headers.entries()].filter(([k]) => k !== "date").sort();
  return { status: r.status, headers, body: await r.text() };
}

function random(n: number): Uint8Array {
  const b = new Uint8Array(n);
  for (let i = 0; i < n; i += 65536) crypto.getRandomValues(b.subarray(i, Math.min(n, i + 65536)));
  return b;
}

describe("public namespace", () => {
  it("stores zstd and raw blobs and serves them to anyone through /has, /get and /blob", async () => {
    const a = enc("function record a ".repeat(50));
    const b = random(3000);
    const absent = await sha(enc("never stored"));
    const res = await putOk("pubWrite", [{ content: a }, { content: b, codec: CODEC_RAW }]);
    expect(res.blobs).toBe(2);
    expect(res.pack).toMatch(/^[0-9a-f]{64}$/);

    const [ha, hb] = [await sha(a), await sha(b)];
    expect(await hasState(undefined, [ha, hb, absent])).toEqual([1, 1, 0]);
    expect(await hasState("privRead", [ha, hb, absent])).toEqual([1, 1, 0]);

    const got = await getBlobs(undefined, [absent, hb, ha]);
    expect(got.missing).toEqual([hex(absent)]);
    expect(got.blobs.map((g) => g.hash).sort()).toEqual([hex(ha), hex(hb)].sort());
    for (const g of got.blobs) expect(hex(await sha(await content(g)))).toBe(g.hash);

    const whole = await call("GET", `/blob/${hex(hb)}`);
    expect(whole.status).toBe(200);
    expect(whole.headers.get("x-blaze-codec")).toBe("raw");
    expect(whole.headers.get("x-blaze-ulength")).toBe("3000");
    expect(new Uint8Array(await whole.arrayBuffer())).toEqual(b);
    const part = await call("GET", `/blob/${hex(hb)}`, { headers: { range: "bytes=100-199" } });
    expect(part.status).toBe(206);
    expect(part.headers.get("content-range")).toBe("bytes 100-199/3000");
    expect(new Uint8Array(await part.arrayBuffer())).toEqual(b.subarray(100, 200));
    expect((await call("GET", `/blob/${hex(hb)}`, { headers: { range: "bytes=3000-" } })).status).toBe(416);
    const head = await call("HEAD", `/blob/${hex(ha)}`);
    expect(head.status).toBe(200);
    expect(head.headers.get("x-blaze-codec")).toBe("zstd");
  });

  it("serves many blobs across several packs in one /get", async () => {
    const blobs = Array.from({ length: 60 }, (_, i) => enc(`blob ${i} `.repeat(i + 1)));
    await putOk("pubWrite", blobs.slice(0, 30).map((content) => ({ content })));
    await putOk("pubWrite", blobs.slice(30).map((content) => ({ content, codec: CODEC_RAW })));
    const hashes = await Promise.all(blobs.map(sha));
    const got = await getBlobs(undefined, hashes);
    expect(got.missing).toEqual([]);
    expect(got.blobs.length).toBe(60);
    for (const g of got.blobs) expect(hex(await sha(await content(g)))).toBe(g.hash);
  });
});

describe("private namespaces", () => {
  it("a second tenant probing a planted private blob gets what an absent one gets", async () => {
    const secret = enc("region code 4417");
    const planted = await sha(secret);
    const absent = await sha(enc("region code 4418"));
    await putOk("privWrite", [{ content: secret }]);

    expect(await hasState("privRead", [planted])).toEqual([2]);
    for (const prober of ["otherWrite", "pubRead", undefined] as const) {
      expect(await hasState(prober, [planted, absent])).toEqual([0, 0]);
      const got = await getBlobs(prober, [planted, absent]);
      expect(got).toEqual({ blobs: [], missing: [hex(planted), hex(absent)] });
      for (const method of ["GET", "HEAD"]) {
        const opts: Opts = { token: prober };
        const a = await shape(await call(method, `/blob/${hex(planted)}`, opts));
        const b = await shape(await call(method, `/blob/${hex(absent)}`, opts));
        const c = await shape(await call(method, `/blob/not-a-hash`, opts));
        const d = await shape(await call(method, `/t/pub/v3/keys/${hex(absent)}`, opts));
        expect(a.status).toBe(404);
        expect(b).toEqual(a);
        expect(c).toEqual(a);
        expect(d).toEqual(a);
      }
    }
    const own = await call("GET", `/blob/${hex(planted)}`, { token: "privRead" });
    expect(own.status).toBe(200);
    expect(own.headers.get("cache-control")).toBe("private, no-cache");
  });

  it("names a private pack and its locations by HMAC, never by a hash", async () => {
    const blobs = [enc("private one"), enc("private two")];
    const res = await putOk("privWrite", blobs.map((content) => ({ content })));
    const hashes = (await Promise.all(blobs.map(sha))).map(hex);

    const objects = (await testEnv.BUCKET.list({ prefix: "cas/t/202/" })).objects;
    expect(objects.map((o) => o.key)).toContain(`cas/t/202/${res.pack}.bpk`);
    const packBytes = new Uint8Array(await (await testEnv.BUCKET.get(`cas/t/202/${res.pack}.bpk`))!.arrayBuffer());
    expect(res.pack).not.toBe(hex(await sha(packBytes)));
    const packHex = hex(packBytes);
    for (const h of hashes) expect(packHex).not.toContain(h);

    const rows = (await testEnv.DB.prepare("SELECT ns, key FROM blobs WHERE object = ?1").bind(`cas/t/202/${res.pack}.bpk`).all<{ ns: string; key: string }>()).results;
    expect(rows.length).toBe(2);
    for (const row of rows) {
      expect(row.ns).toBe("202");
      expect(hashes).not.toContain(row.key);
    }
    const leaked = await testEnv.DB.prepare("SELECT count(*) AS n FROM blobs WHERE key IN (?1, ?2)").bind(...hashes).first<{ n: number }>();
    expect(leaked!.n).toBe(0);
  });

  it("stores the same bytes once per tenant, and each tenant sees only its own", async () => {
    const same = enc("the same bytes in two tenants");
    const h = await sha(same);
    const a = await putOk("privWrite", [{ content: same }]);
    const b = await putOk("otherWrite", [{ content: same }]);
    expect(a.pack).not.toBe(b.pack);
    expect(await hasState("privRead", [h])).toEqual([2]);
    expect(await hasState("otherWrite", [h])).toEqual([2]);
    expect(await hasState(undefined, [h])).toEqual([0]);
  });

  it("reads public blobs too, reporting them as public", async () => {
    const pub = enc("a public dependency unit");
    await putOk("pubWrite", [{ content: pub }]);
    const h = await sha(pub);
    expect(await hasState("privRead", [h])).toEqual([1]);
    const got = await getBlobs("privRead", [h]);
    expect(got.blobs.map((g) => g.hash)).toEqual([hex(h)]);
  });
});

describe("/put refusals store nothing", () => {
  async function stored(): Promise<number> {
    const objects = (await testEnv.BUCKET.list({ prefix: "cas/" })).objects.length;
    const rows = (await testEnv.DB.prepare("SELECT count(*) AS n FROM blobs").first<{ n: number }>())!.n;
    return objects + rows;
  }

  const good = enc("a good blob");
  const cases: [string, () => Promise<Blob[]>, number][] = [
    ["a frame that does not hash to its name", async () => [{ content: good }, { content: enc("x"), hash: await sha(enc("y")) }], 422],
    ["a length that is not its ulen", async () => [{ content: good, ulen: good.length + 1 }], 422],
    ["a raw frame shorter than its ulen", async () => [{ content: good, codec: CODEC_RAW, ulen: good.length + 5 }], 422],
    ["a ulen over 4 MiB", async () => [{ content: good, ulen: 4 * 1024 * 1024 + 1 }], 422],
    ["a zstd frame that decompresses past its ulen", async () => {
      const big = new Uint8Array(100_000);
      return [{ content: big, hash: await sha(big.subarray(0, 10)), ulen: 10 }];
    }, 422],
    ["bytes that are not zstd", async () => [{ content: good, data: enc("not zstd at all") }], 422],
    ["a dictionary", async () => [{ content: good, dict: new Uint8Array(32) }], 422],
  ];
  for (const [what, blobs, status] of cases) {
    it(`refuses ${what} with ${status}`, async () => {
      const before = await stored();
      const r = await put("pubWrite", await stream(await blobs()));
      expect(r.status, r.status === 200 ? "" : await r.clone().text()).toBe(status);
      expect(await stored()).toBe(before);
    });
  }

  it("refuses a stream without its end record, or with junk after it, with 400", async () => {
    const before = await stored();
    expect((await put("pubWrite", await stream([{ content: good }], { end: false }))).status).toBe(400);
    const junk = await stream([{ content: good }]);
    expect((await put("pubWrite", new Uint8Array([...junk, 1]))).status).toBe(400);
    expect((await put("pubWrite", enc("BLZSTRM1"))).status).toBe(400);
    expect((await put("pubWrite", enc("not a stream"))).status).toBe(400);
    expect(await stored()).toBe(before);
  });

  it("refuses anonymous with 401, a read token with 403, and over 32 MiB with 413", async () => {
    const body = await stream([{ content: good }]);
    expect((await put(undefined, body)).status).toBe(401);
    expect((await put("pubRead", body)).status).toBe(403);
    expect((await put("privRead", body)).status).toBe(403);
    const request = new Request(`${BASE}/put`, {
      method: "POST",
      headers: { authorization: `Bearer ${TOKENS.pubWrite.token}`, "content-length": String(MAX_PUT + 1) },
      body: new ReadableStream({ start: (c) => c.close() }),
      duplex: "half",
    } as RequestInit);
    expect((await handle(request, testEnv)).status).toBe(413);
  });
});

describe("/has and /get bodies", () => {
  it("refuses a malformed body with 400", async () => {
    for (const body of [enc("junk"), encode({ hashes: [new Uint8Array(31)] }), encode({ other: [] }), encode({ hashes: "x" })]) {
      expect((await call("POST", "/has", { body })).status).toBe(400);
      expect((await call("POST", "/get", { body })).status).toBe(400);
    }
  });

  it("refuses more than 20,000 hashes", async () => {
    const hashes = Array.from({ length: 20_001 }, () => new Uint8Array(32));
    expect([400, 413]).toContain((await call("POST", "/has", { body: encode({ hashes }) })).status);
  });

  it("answers 405 for other methods", async () => {
    expect((await call("GET", "/has")).status).toBe(405);
    expect((await call("PUT", "/put", { token: "pubWrite", body: "x" })).status).toBe(405);
    expect((await call("DELETE", `/blob/${"0".repeat(64)}`, { token: "pubWrite" })).status).toBe(405);
  });
});

describe("cbor", () => {
  it("encodes maps in deterministic key order and round-trips", () => {
    const v: Cbor = { zz: 1, a: [1, 24, 255, 256, 65536, 2 ** 32, new Uint8Array([1, 2])], b: { y: true, x: null } };
    const bytes = encode(v);
    expect(encode(decode(bytes))).toEqual(bytes);
    expect(hex(encode({ b: 0, a: 0, aa: 0 }))).toBe("a3616100616200626161" + "00");
  });

  it("refuses non-shortest integers, indefinite lengths, unsorted keys and trailing bytes", () => {
    for (const bad of ["1817", "5f4100ff", "a2616201616101", "0000", "9f01ff", "fa3f800000"]) {
      const bytes = Uint8Array.from(bad.match(/../g)!.map((x) => parseInt(x, 16)));
      expect(() => decode(bytes)).toThrow(CborError);
    }
  });
});
