import { describe, expect, it } from "vitest";
import { sha256Hex } from "../src/auth";
import { handle, MAX_BODY, type Env } from "../src/index";
import { BASE, call, testEnv, TOKENS, type Opts, type TokenName } from "./fixture";

const enc = (s: string) => new TextEncoder().encode(s);
const str = async (r: Response) => new TextDecoder().decode(await r.arrayBuffer());

async function hexOf(bytes: Uint8Array): Promise<string> {
  return sha256Hex(bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer);
}

/** Every observable part of a response except the date. */
async function shape(r: Response) {
  const headers = [...r.headers.entries()].filter(([k]) => k !== "date").sort();
  return { status: r.status, headers, body: await r.text() };
}

async function putKey(tenant: string, token: TokenName, body: string) {
  const bytes = enc(body);
  const h = await hexOf(bytes);
  const r = await call("PUT", `/t/${tenant}/v3/keys/${h}`, { token, body: bytes });
  expect(r.status).toBe(200);
  return h;
}

describe("the one 404", () => {
  it("is byte-identical for absent, private without its token, wrong tenant, unknown tenant and bad names", async () => {
    const planted = await putKey("priv", "privWrite", "a private key list");
    const absent = "0".repeat(64);
    for (const method of ["GET", "HEAD"]) {
      const cases: [string, Opts][] = [
        [`/t/pub/v3/keys/${absent}`, {}],
        [`/t/pub/v3/keys/${absent}`, { token: "pubRead" }],
        [`/t/priv/v3/keys/${absent}`, { token: "privRead" }],
        [`/t/priv/v3/keys/${planted}`, {}],
        [`/t/priv/v3/keys/${planted}`, { token: "otherWrite" }],
        [`/t/priv/v3/keys/${planted}`, { token: "pubRead" }],
        [`/t/nosuch/v3/keys/${planted}`, {}],
        [`/t/nosuch/v3/keys/${planted}`, { token: "privRead" }],
        [`/t/NOT_A_TENANT/v3/keys/${planted}`, {}],
        [`/t/pub/v3/keys/${planted.toUpperCase()}`, {}],
        [`/t/pub/v3/keys/abc`, {}],
        [`/t/pub/v3/packs/${absent}.tar`, {}],
        [`/t/pub/v3/secrets/${absent}`, {}],
        [`/t/priv/v3/manifests/a%2Fb`, { token: "privRead" }],
        [`/t/pub/v3/manifests/${"x".repeat(129)}`, {}],
        [`/nothing/here`, {}],
      ];
      const shapes = [];
      for (const [path, opts] of cases) shapes.push(await shape(await call(method, path, opts)));
      expect(shapes[0].status).toBe(404);
      for (const s of shapes) expect(s).toEqual(shapes[0]);
    }
    // Writes to a private tenant with another tenant's token get the same 404 too.
    const write = await shape(
      await call("PUT", `/t/priv/v3/keys/${planted}`, { token: "otherWrite", body: "a private key list" }),
    );
    expect(write).toEqual(await shape(await call("GET", `/t/pub/v3/keys/${absent}`)));
  });

  it("serves a private tenant's own object with its read token", async () => {
    const h = await putKey("priv", "privWrite", "mine");
    const r = await call("GET", `/t/priv/v3/keys/${h}`, { token: "privRead" });
    expect(r.status).toBe(200);
    expect(await str(r)).toBe("mine");
    expect(r.headers.get("cache-control")).toBe("private, no-cache");
  });
});

describe("auth", () => {
  it("refuses anonymous writes with 401", async () => {
    const r = await call("PUT", `/t/pub/v3/keys/${"1".repeat(64)}`, { body: "x" });
    expect(r.status).toBe(401);
  });

  it("refuses a read token's write with 403 and stores nothing", async () => {
    const bytes = enc("read-only");
    const h = await hexOf(bytes);
    expect((await call("PUT", `/t/pub/v3/keys/${h}`, { token: "pubRead", body: bytes })).status).toBe(403);
    expect((await call("GET", `/t/pub/v3/keys/${h}`)).status).toBe(404);
  });

  it("refuses an expired token with 401, reads included", async () => {
    expect((await call("GET", `/t/priv/v3/keys/${"1".repeat(64)}`, { token: "expired" })).status).toBe(401);
    expect((await call("GET", `/t/pub/v3/keys/${"1".repeat(64)}`, { token: "expired" })).status).toBe(401);
    expect((await call("PUT", `/t/priv/v3/keys/${"1".repeat(64)}`, { token: "expired", body: "x" })).status).toBe(401);
  });

  it("refuses an unknown or malformed token with 401, never as anonymous", async () => {
    const path = `/t/pub/v3/keys/${"1".repeat(64)}`;
    expect((await call("GET", path, { headers: { authorization: "Bearer nope" } })).status).toBe(401);
    expect((await call("GET", path, { headers: { authorization: "Basic abc" } })).status).toBe(401);
  });

  it("answers 403 for another tenant's write token on a public tenant", async () => {
    const r = await call("PUT", `/t/pub/v3/keys/${"1".repeat(64)}`, { token: "privWrite", body: "x" });
    expect(r.status).toBe(403);
  });
});

describe("PUT", () => {
  it("stores a content-addressed object and serves it with its headers", async () => {
    const bytes = enc("pack bytes");
    const h = await hexOf(bytes);
    const put = await call("PUT", `/t/pub/v3/packs/${h}.pack`, { token: "pubWrite", body: bytes });
    expect(put.status).toBe(200);
    const { etag } = (await put.json()) as { etag: string };
    expect(put.headers.get("etag")).toBe(`"${etag}"`);

    const get = await call("GET", `/t/pub/v3/packs/${h}.pack`);
    expect(get.status).toBe(200);
    expect(await str(get)).toBe("pack bytes");
    expect(get.headers.get("etag")).toBe(`"${etag}"`);
    expect(get.headers.get("content-length")).toBe(String(bytes.length));
    expect(get.headers.get("cache-control")).toBe("public, max-age=31536000, immutable");
    const uploaded = Number(get.headers.get("x-blaze-uploaded"));
    expect(Math.abs(uploaded - Date.now() / 1000)).toBeLessThan(120);
    expect(new Date(get.headers.get("last-modified")!).getTime() / 1000).toBe(uploaded);

    const head = await call("HEAD", `/t/pub/v3/packs/${h}.pack`);
    expect(head.status).toBe(200);
    expect(head.headers.get("content-length")).toBe(String(bytes.length));
    expect(head.headers.get("etag")).toBe(`"${etag}"`);
    expect(await str(head)).toBe("");
  });

  for (const prefix of ["packs/", "keys/", "units/"]) {
    it(`answers 422 and stores nothing when a ${prefix} body does not hash to its name`, async () => {
      const h = await hexOf(enc("claimed"));
      const name = prefix === "packs/" ? `packs/${h}.pack` : `${prefix}${h}`;
      const r = await call("PUT", `/t/pub/v3/${name}`, { token: "pubWrite", body: "poison" });
      expect(r.status).toBe(422);
      expect(await str(r)).toBe("hash mismatch\n");
      expect((await call("GET", `/t/pub/v3/${name}`)).status).toBe(404);
      expect(await testEnv.BUCKET.head(`v3/101/${name}`)).toBeNull();
    });
  }

  it("checks a .pack.zst against x-blaze-sha256 and returns it", async () => {
    const pack = await hexOf(enc("uncompressed pack"));
    const body = enc("compressed bytes");
    const sha = await hexOf(body);
    const path = `/t/pub/v3/packs/${pack}.pack.zst`;
    expect((await call("PUT", path, { token: "pubWrite", body })).status).toBe(400);
    const wrong = await call("PUT", path, { token: "pubWrite", body, headers: { "x-blaze-sha256": pack } });
    expect(wrong.status).toBe(422);
    expect((await call("GET", path)).status).toBe(404);
    const ok = await call("PUT", path, { token: "pubWrite", body, headers: { "x-blaze-sha256": sha } });
    expect(ok.status).toBe(200);
    const get = await call("GET", path);
    expect(get.headers.get("x-blaze-sha256")).toBe(sha);
    expect(await str(get)).toBe("compressed bytes");
    expect((await call("HEAD", path)).headers.get("x-blaze-sha256")).toBe(sha);
  });

  it("answers 413 over 90 MiB without reading the body", async () => {
    const request = new Request(`${BASE}/t/pub/v3/keys/${"1".repeat(64)}`, {
      method: "PUT",
      headers: { authorization: `Bearer ${TOKENS.pubWrite.token}`, "content-length": String(MAX_BODY + 1) },
      body: new ReadableStream({ start: (c) => c.close() }),
      duplex: "half",
    } as RequestInit);
    const r = await handle(request, testEnv);
    expect(r.status).toBe(413);
  });
});

describe("conditional writes", () => {
  const path = "/t/pub/v3/manifests/blaze";

  it("If-None-Match: * creates once, then 412", async () => {
    const first = await call("PUT", path, { token: "pubWrite", body: "v1", headers: { "if-none-match": "*" } });
    expect(first.status).toBe(200);
    const second = await call("PUT", path, { token: "pubWrite", body: "v2", headers: { "if-none-match": "*" } });
    expect(second.status).toBe(412);
    expect(await str(await call("GET", path))).toBe("v1");
  });

  it("If-Match takes the bare or the quoted ETag, and a stale one is 412", async () => {
    const v1 = (await (await call("PUT", path, { token: "pubWrite", body: "v1" })).json()) as { etag: string };
    const v2 = await call("PUT", path, { token: "pubWrite", body: "v2", headers: { "if-match": v1.etag } });
    expect(v2.status).toBe(200);
    const stale = await call("PUT", path, { token: "pubWrite", body: "v3", headers: { "if-match": `"${v1.etag}"` } });
    expect(stale.status).toBe(412);
    const quoted = await call("PUT", path, { token: "pubWrite", body: "v3", headers: { "if-match": v2.headers.get("etag")! } });
    expect(quoted.status).toBe(200);
    expect(await str(await call("GET", path))).toBe("v3");
  });

  it("lets exactly one of two racing writers on the same ETag win", async () => {
    const base = (await (await call("PUT", path, { token: "pubWrite", body: "base" })).json()) as { etag: string };
    const [a, b] = await Promise.all(
      ["a", "b"].map((body) => call("PUT", path, { token: "pubWrite", body, headers: { "if-match": base.etag } })),
    );
    expect([a.status, b.status].sort()).toEqual([200, 412]);
    const winner = a.status === 200 ? "a" : "b";
    expect(await str(await call("GET", path))).toBe(winner);
  });

  it("If-Match on an absent object is 412", async () => {
    const r = await call("PUT", "/t/pub/v3/deps/none", { token: "pubWrite", body: "x", headers: { "if-match": "abc" } });
    expect(r.status).toBe(412);
  });
});

describe("ranges", () => {
  async function stored(): Promise<string> {
    const body = enc("0123456789");
    const h = await hexOf(body);
    await call("PUT", `/t/pub/v3/units/${h}`, { token: "pubWrite", body });
    return `/t/pub/v3/units/${h}`;
  }

  it("serves bytes=a-b and bytes=a- as 206", async () => {
    const path = await stored();
    const r = await call("GET", path, { headers: { range: "bytes=2-5" } });
    expect(r.status).toBe(206);
    expect(r.headers.get("content-range")).toBe("bytes 2-5/10");
    expect(r.headers.get("content-length")).toBe("4");
    expect(await str(r)).toBe("2345");
    const open = await call("GET", path, { headers: { range: "bytes=7-" } });
    expect(open.status).toBe(206);
    expect(open.headers.get("content-range")).toBe("bytes 7-9/10");
    expect(await str(open)).toBe("789");
  });

  it("clamps an end past the size", async () => {
    const path = await stored();
    const r = await call("GET", path, { headers: { range: "bytes=8-100" } });
    expect(r.status).toBe(206);
    expect(r.headers.get("content-range")).toBe("bytes 8-9/10");
    expect(await str(r)).toBe("89");
  });

  for (const range of ["bytes=10-", "bytes=11-20", "bytes=5-2", "bytes=-3", "items=0-1", "bytes=0-1,3-4"]) {
    it(`answers 416 for ${range}`, async () => {
      const path = await stored();
      const r = await call("GET", path, { headers: { range } });
      expect(r.status).toBe(416);
      expect(r.headers.get("content-range")).toBe("bytes */10");
    });
  }

  it("answers the one 404 for a range of an absent object", async () => {
    const r = await call("GET", `/t/pub/v3/units/${"2".repeat(64)}`, { headers: { range: "bytes=0-1" } });
    expect(await shape(r)).toEqual(await shape(await call("GET", `/t/pub/v3/units/${"3".repeat(64)}`)));
  });
});

describe("multipart", () => {
  const MIB = 1024 * 1024;

  function bytesOf(size: number, seed: number): Uint8Array {
    const b = new Uint8Array(size);
    for (let i = 0; i < size; i++) b[i] = (i * 31 + seed) & 0xff;
    return b;
  }

  async function uploadParts(path: string, parts: Uint8Array[], headers: Record<string, string> = {}) {
    const start = await call("POST", `${path}?uploads`, { token: "pubWrite" });
    expect(start.status).toBe(200);
    const { upload } = (await start.json()) as { upload: string };
    const done = [];
    for (const [i, part] of parts.entries()) {
      const r = await call("PUT", `${path}?upload=${encodeURIComponent(upload)}&part=${i + 1}`, { token: "pubWrite", body: part });
      expect(r.status).toBe(200);
      done.push({ part: i + 1, etag: ((await r.json()) as { etag: string }).etag });
    }
    return { upload, done };
  }

  async function staging(): Promise<string[]> {
    return (await testEnv.BUCKET.list({ prefix: "staging/" })).objects.map((o) => o.key);
  }

  it("assembles parts, checks the hash, and removes the staging object", async () => {
    const parts = [bytesOf(5 * MIB, 1), bytesOf(1234, 2)];
    const whole = new Uint8Array(5 * MIB + 1234);
    whole.set(parts[0]);
    whole.set(parts[1], 5 * MIB);
    const h = await hexOf(whole);
    const path = `/t/pub/v3/packs/${h}.pack`;
    const { upload, done } = await uploadParts(path, parts);
    const r = await call("POST", `${path}?upload=${encodeURIComponent(upload)}`, {
      token: "pubWrite",
      body: JSON.stringify({ parts: done, sha256: h }),
    });
    expect(r.status).toBe(200);
    const get = await call("GET", path);
    expect(get.headers.get("content-length")).toBe(String(whole.length));
    expect(await hexOf(new Uint8Array(await get.arrayBuffer()))).toBe(h);
    expect(await staging()).toEqual([]);
  });

  it("answers 422 when the assembled body does not hash to the name, and removes staging", async () => {
    const parts = [bytesOf(5 * MIB, 3), bytesOf(10, 4)];
    const h = await hexOf(enc("something else"));
    const path = `/t/pub/v3/units/${h}`;
    const { upload, done } = await uploadParts(path, parts);
    const r = await call("POST", `${path}?upload=${encodeURIComponent(upload)}`, {
      token: "pubWrite",
      body: JSON.stringify({ parts: done, sha256: h }),
    });
    expect(r.status).toBe(422);
    expect((await call("GET", path)).status).toBe(404);
    expect(await staging()).toEqual([]);
  });

  it("answers 422 when the declared sha256 is not the name's", async () => {
    const parts = [bytesOf(100, 5)];
    const h = await hexOf(parts[0]);
    const path = `/t/pub/v3/units/${h}`;
    const { upload, done } = await uploadParts(path, parts);
    const r = await call("POST", `${path}?upload=${encodeURIComponent(upload)}`, {
      token: "pubWrite",
      body: JSON.stringify({ parts: done, sha256: "4".repeat(64) }),
    });
    expect(r.status).toBe(422);
    expect((await call("GET", path)).status).toBe(404);
  });

  it("checks a .pack.zst against x-blaze-sha256 on completion", async () => {
    const parts = [bytesOf(300, 6)];
    const sha = await hexOf(parts[0]);
    const path = `/t/pub/v3/packs/${"5".repeat(64)}.pack.zst`;
    const { upload, done } = await uploadParts(path, parts);
    const r = await call("POST", `${path}?upload=${encodeURIComponent(upload)}`, {
      token: "pubWrite",
      body: JSON.stringify({ parts: done, sha256: sha }),
      headers: { "x-blaze-sha256": sha },
    });
    expect(r.status).toBe(200);
    expect((await call("HEAD", path)).headers.get("x-blaze-sha256")).toBe(sha);
  });

  it("aborts with 204", async () => {
    const path = `/t/pub/v3/units/${"6".repeat(64)}`;
    const { upload } = await uploadParts(path, [bytesOf(10, 7)]);
    const r = await call("DELETE", `${path}?upload=${encodeURIComponent(upload)}`, { token: "pubWrite" });
    expect(r.status).toBe(204);
  });

  it("is only for packs/ and units/", async () => {
    const r = await call("POST", `/t/pub/v3/keys/${"7".repeat(64)}?uploads`, { token: "pubWrite" });
    expect(r.status).toBe(405);
  });

  it("refuses a forged upload id", async () => {
    const path = `/t/pub/v3/units/${"8".repeat(64)}`;
    const r = await call("PUT", `${path}?upload=../../x&part=1`, { token: "pubWrite", body: "x" });
    expect(r.status).toBe(400);
  });
});

describe("other requests", () => {
  it("answers 405 for DELETE of an object, listings and other methods", async () => {
    const h = await putKey("pub", "pubWrite", "keep me");
    expect((await call("DELETE", `/t/pub/v3/keys/${h}`, { token: "pubWrite" })).status).toBe(405);
    expect((await call("GET", `/t/pub/v3/`)).status).toBe(405);
    expect((await call("GET", `/t/pub/v3/keys/`)).status).toBe(405);
    expect((await call("PATCH", `/t/pub/v3/keys/${h}`, { token: "pubWrite", body: "x" })).status).toBe(405);
    expect((await call("POST", `/t/pub/v3/keys/${h}`, { token: "pubWrite" })).status).toBe(405);
    expect((await call("GET", `/t/pub/v3/keys/${h}`)).status).toBe(200);
  });
});

describe("rate limit and meter", () => {
  function fakeEnv(success: boolean) {
    const points: AnalyticsEngineDataPoint[] = [];
    const limiter = { limit: async () => ({ success }) };
    const e: Env = { ...testEnv, RL_TENANT: limiter, RL_ANON: limiter, METER: { writeDataPoint: (p) => points.push(p!) } };
    return { e, points };
  }

  it("answers 429 with Retry-After", async () => {
    const { e } = fakeEnv(false);
    const r = await handle(new Request(`${BASE}/t/pub/v3/keys/${"1".repeat(64)}`), e);
    expect(r.status).toBe(429);
    expect(Number(r.headers.get("retry-after"))).toBeGreaterThan(0);
  });

  it("writes one data point per request with tenant, op, status and bytes", async () => {
    const { e, points } = fakeEnv(true);
    const body = enc("metered");
    const h = await hexOf(body);
    const put = new Request(`${BASE}/t/pub/v3/keys/${h}`, {
      method: "PUT",
      body,
      headers: { authorization: `Bearer ${TOKENS.pubWrite.token}`, "content-length": String(body.length) },
    });
    expect((await handle(put, e)).status).toBe(200);
    const get = await handle(new Request(`${BASE}/t/pub/v3/keys/${h}`), e);
    await get.arrayBuffer();
    await handle(new Request(`${BASE}/t/nosuch/v3/keys/${h}`), e);
    expect(points).toEqual([
      { indexes: ["pub"], blobs: ["pub", "put"], doubles: [200, body.length, 0] },
      { indexes: ["pub"], blobs: ["pub", "get"], doubles: [200, 0, body.length] },
      { indexes: ["-"], blobs: ["-", "get"], doubles: [404, 0, 0] },
    ]);
  });
});
