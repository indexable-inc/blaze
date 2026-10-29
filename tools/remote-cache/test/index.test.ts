import { afterEach, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { hex } from "../src/cas";
import { decode, encode, type Cbor } from "../src/cbor";
import { clock, DAY } from "../src/clock";
import { handle } from "../src/index";
import { blobRecordHeader, CODEC_RAW, STREAM_MAGIC, TAG_END } from "../src/pack";
import { HEAD_CONTEXT, KEYS_CONTEXT } from "../src/schema";
import { BASE, TENANTS, testEnv, TOKENS, type TokenName } from "./fixture";

const enc = (s: string) => new TextEncoder().encode(s);
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

async function sha(bytes: Uint8Array): Promise<Uint8Array> {
  return new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
}

interface Opts {
  token?: TokenName;
  body?: Uint8Array;
  headers?: Record<string, string>;
}

/** Straight into `handle`, so the test's `clock` is the gateway's. */
function call(method: string, path: string, opts: Opts = {}): Promise<Response> {
  const headers: Record<string, string> = { ...opts.headers };
  if (opts.token) headers.authorization = `Bearer ${TOKENS[opts.token].token}`;
  return handle(new Request(`${BASE}${path}`, { method, headers, body: opts.body }), testEnv);
}

async function shape(r: Response) {
  const headers = [...r.headers.entries()].filter(([k]) => k !== "date").sort();
  return { status: r.status, headers, body: await r.text() };
}

async function cborOf(r: Response): Promise<any> {
  expect(r.status, r.status === 200 ? "" : await r.clone().text()).toBe(200);
  return decode(new Uint8Array(await r.arrayBuffer()));
}

// ---- keys and signing

interface Signer {
  name: string;
  pair: CryptoKeyPair;
  pub: Uint8Array;
}

async function signer(name: string): Promise<Signer> {
  const pair = (await crypto.subtle.generateKey({ name: "Ed25519" }, true, ["sign", "verify"])) as CryptoKeyPair;
  const pub = new Uint8Array((await crypto.subtle.exportKey("raw", pair.publicKey)) as ArrayBuffer);
  return { name, pair, pub };
}

async function envelope(s: Signer, context: Uint8Array, body: Uint8Array): Promise<Uint8Array> {
  const msg = new Uint8Array(context.length + body.length);
  msg.set(context);
  msg.set(body, context.length);
  const sig = new Uint8Array(await crypto.subtle.sign({ name: "Ed25519" }, s.pair.privateKey, msg));
  return encode({ body, sig });
}

let roots: Record<"pub" | "priv" | "other", Signer>;
let ci: Signer;
let ci2: Signer;
const now = () => clock.nowSeconds();

beforeAll(async () => {
  roots = { pub: await signer("pub-root-1"), priv: await signer("priv-root-1"), other: await signer("other-root-1") };
  ci = await signer("ci-1");
  ci2 = await signer("ci-2");
});

beforeEach(async () => {
  for (const prefix of ["trust/", "index/"]) {
    const keys = (await testEnv.BUCKET.list({ prefix })).objects.map((o) => o.key);
    if (keys.length) await testEnv.BUCKET.delete(keys);
  }
  for (const t of ["pub", "priv", "other"] as const) {
    await testEnv.TOKENS.put(`tenant:${t}`, JSON.stringify({ ...TENANTS[t], root: { name: roots[t].name, key: hex(roots[t].pub) } }));
  }
});

afterEach(() => {
  clock.offsetMs = 0;
});

type KeySpec = { s: Signer; not_before?: number; not_after?: number };

async function keyList(tenant: string, turn: number, keys: KeySpec[], root?: Signer, listTenant = tenant): Promise<Uint8Array> {
  const body = encode({
    tenant: listTenant,
    turn,
    signer: (root ?? roots[tenant as "pub"]).name,
    keys: keys.map((k) => ({ name: k.s.name, key: k.s.pub, role: "heads", not_before: k.not_before ?? now() - DAY, not_after: k.not_after ?? now() + 60 * DAY })),
  });
  return envelope(root ?? roots[tenant as "pub"], KEYS_CONTEXT, body);
}

async function putTrust(tenant: string, token: TokenName, list: Uint8Array): Promise<Response> {
  return call("PUT", `/t/${tenant}/trust`, { token, body: list });
}

async function trusted(tenant: "pub" | "priv" | "other", token: TokenName, keys: KeySpec[] = [{ s: ci }], turn = 1) {
  const r = await putTrust(tenant, token, await keyList(tenant, turn, keys));
  expect(r.status, await r.clone().text()).toBe(200);
}

interface HeadSpec {
  tenant: string;
  name: string;
  turn: number;
  segments: Uint8Array[];
  signer?: Signer;
  created?: number;
}

function headBody(h: HeadSpec): Uint8Array {
  return encode({
    tenant: h.tenant,
    name: h.name,
    turn: h.turn,
    created: h.created ?? now(),
    versions: { store: 4, codegen: 41 },
    toolchain: "nightly-2026-10-25",
    target: "x86_64-unknown-linux-gnu",
    segments: h.segments,
    signer: (h.signer ?? ci).name,
  });
}

async function signedHead(h: HeadSpec): Promise<Uint8Array> {
  return envelope(h.signer ?? ci, HEAD_CONTEXT, headBody(h));
}

function putHead(tenant: string, name: string, token: TokenName, body: Uint8Array, cond: { ifMatch?: string; new?: boolean } = { new: true }) {
  const headers: Record<string, string> = {};
  if (cond.ifMatch !== undefined) headers["if-match"] = cond.ifMatch;
  if (cond.new) headers["if-none-match"] = "*";
  return call("PUT", `/t/${tenant}/head/${name}`, { token, body, headers });
}

async function readHeads(tenant: string, names: string[], token?: TokenName) {
  return (await cborOf(await call("POST", `/t/${tenant}/heads`, { token, body: encode({ names }) }))).heads as ({ etag: string; head: Uint8Array } | null)[];
}

// ---- blobs and segments

async function putBlobs(token: TokenName, contents: Uint8Array[]): Promise<{ hashes: Uint8Array[]; pack: string }> {
  const parts: Uint8Array[] = [STREAM_MAGIC];
  const hashes: Uint8Array[] = [];
  for (const c of contents) {
    const h = await sha(c);
    hashes.push(h);
    parts.push(blobRecordHeader(h, CODEC_RAW, null, c.length, c.length), c);
  }
  parts.push(new Uint8Array([TAG_END]));
  const body = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let pos = 0;
  for (const p of parts) {
    body.set(p, pos);
    pos += p.length;
  }
  const r = await call("POST", "/put", { token, body, headers: { "content-length": String(body.length) } });
  const { pack } = await cborOf(r);
  return { hashes, pack };
}

function segment(entries: [string, Uint8Array, [Uint8Array, number][]][]): Uint8Array {
  return encode({ v: 1, entries: entries.map(([kind, key, blobs]) => [kind, key, blobs, new Uint8Array(0)]) as Cbor[] });
}

async function putSeg(tenant: string, token: TokenName, body: Uint8Array): Promise<Response> {
  return call("PUT", `/t/${tenant}/seg`, { token, body });
}

async function segOk(tenant: string, token: TokenName, body: Uint8Array): Promise<Uint8Array> {
  return (await cborOf(await putSeg(tenant, token, body))).seg;
}

/** A public blob and a public segment naming it. */
async function pubFixture(label: string) {
  const content = enc(`public blob ${label}`);
  const { hashes, pack } = await putBlobs("pubWrite", [content]);
  const seg = await segOk("pub", "pubWrite", segment([["fn", enc(label), [[hashes[0], content.length]]]]));
  return { hash: hashes[0], pack, seg, ulen: content.length };
}

// ---- tests

describe("trust", () => {
  it("stores a key list signed by the tenant's root, readable as stored, and only ever at a higher turn", async () => {
    const list = await keyList("pub", 1, [{ s: ci }]);
    expect((await putTrust("pub", "pubWrite", list)).status).toBe(200);
    const got = await call("GET", "/t/pub/trust");
    expect(got.status).toBe(200);
    expect(new Uint8Array(await got.arrayBuffer())).toEqual(list);

    for (const turn of [1, 0]) {
      const r = await putTrust("pub", "pubWrite", await keyList("pub", turn, [{ s: ci2 }]));
      expect(r.status).toBe(409);
    }
    expect((await putTrust("pub", "pubWrite", await keyList("pub", 2, [{ s: ci }, { s: ci2 }]))).status).toBe(200);
  });

  it("refuses a list not signed by the registered root, or signed for another tenant", async () => {
    const r1 = await putTrust("priv", "privWrite", await keyList("priv", 1, [{ s: ci }], roots.other));
    expect(r1.status).toBe(422);
    const forged = encode({ ...(decode(await keyList("priv", 1, [{ s: ci }])) as any), sig: new Uint8Array(64) });
    expect((await putTrust("priv", "privWrite", forged)).status).toBe(422);
    const r2 = await putTrust("priv", "privWrite", await keyList("priv", 1, [{ s: ci }], roots.priv, "other"));
    expect(r2.status).toBe(422);
    expect(await r2.text()).toContain("signed for tenant other");
    // other's own list, replayed into priv
    const r3 = await putTrust("priv", "privWrite", await keyList("other", 1, [{ s: ci }]));
    expect(r3.status).toBe(422);
  });

  it("refuses every key list for a tenant with no registered root, saying why", async () => {
    await testEnv.TOKENS.put("tenant:pub", JSON.stringify(TENANTS.pub));
    const r = await putTrust("pub", "pubWrite", await keyList("pub", 1, [{ s: ci }]));
    expect(r.status).toBe(403);
    expect(await r.text()).toContain("no root key registered");
  });

  it("is the one 404 to anyone but the private tenant itself", async () => {
    await trusted("priv", "privWrite");
    expect((await call("GET", "/t/priv/trust", { token: "privRead" })).status).toBe(200);
    const absent = await shape(await call("GET", "/t/other/trust", { token: "otherWrite" }));
    expect(absent.status).toBe(404);
    for (const token of [undefined, "otherWrite", "pubRead"] as const) {
      expect(await shape(await call("GET", "/t/priv/trust", { token }))).toEqual(absent);
    }
    expect(await shape(await call("GET", "/t/nosuch/trust"))).toEqual(absent);
  });
});

describe("segments", () => {
  it("are named by the SHA-256 the gateway computes and served back to readers", async () => {
    const { seg, hash, ulen } = await pubFixture("a");
    const body = segment([["fn", enc("a"), [[hash, ulen]]]]);
    expect(hex(seg)).toBe(hex(await sha(body)));
    const r = await call("GET", `/t/pub/seg/${hex(seg)}`);
    expect(r.status).toBe(200);
    expect(r.headers.get("cache-control")).toContain("immutable");
    expect(new Uint8Array(await r.arrayBuffer())).toEqual(body);
  });

  it("cannot be overwritten: a second put of the same bytes leaves the object, other bytes get another name", async () => {
    const { seg, hash, ulen } = await pubFixture("b");
    const before = (await testEnv.BUCKET.head(`index/seg/101/${hex(seg)}`))!;
    await sleep(5);
    expect(hex(await segOk("pub", "pubWrite", segment([["fn", enc("b"), [[hash, ulen]]]])))).toBe(hex(seg));
    const after = (await testEnv.BUCKET.head(`index/seg/101/${hex(seg)}`))!;
    expect(after.uploaded.getTime()).toBe(before.uploaded.getTime());
    const other = await segOk("pub", "pubWrite", segment([["fn", enc("b2"), [[hash, ulen]]]]));
    expect(hex(other)).not.toBe(hex(seg));
    expect((await call("PUT", `/t/pub/seg/${hex(seg)}`, { token: "pubWrite", body: new Uint8Array(1) })).status).toBe(405);
  });

  it("refuses a segment naming a blob the tenant cannot read, the same way as one never stored", async () => {
    const { hashes } = await putBlobs("otherWrite", [enc("other's private blob")]);
    const never = await sha(enc("never stored"));
    const a = await putSeg("priv", "privWrite", segment([["fn", enc("x"), [[hashes[0], 20]]]]));
    const b = await putSeg("priv", "privWrite", segment([["fn", enc("x"), [[never, 20]]]]));
    expect(a.status).toBe(422);
    expect(b.status).toBe(422);
    expect((await a.text()).replace(hex(hashes[0]), "H")).toBe((await b.text()).replace(hex(never), "H"));
    // and a public tenant cannot name a private blob either
    expect((await putSeg("pub", "pubWrite", segment([["fn", enc("x"), [[hashes[0], 20]]]]))).status).toBe(422);
  });

  it("lets a private tenant name its own and public blobs, under HMAC names", async () => {
    const pub = await pubFixture("c");
    const mine = enc("priv's own blob");
    const { hashes } = await putBlobs("privWrite", [mine]);
    const seg = await segOk("priv", "privWrite", segment([["fn", enc("c"), [[hashes[0], mine.length], [pub.hash, pub.ulen]]]]));
    const listed = (await testEnv.BUCKET.list({ prefix: "index/seg/202/" })).objects.map((o) => o.key);
    expect(listed.length).toBeGreaterThan(0);
    expect(listed).not.toContain(`index/seg/202/${hex(seg)}`);
    expect((await call("GET", `/t/priv/seg/${hex(seg)}`, { token: "privRead" })).status).toBe(200);
  });

  it("refuses malformed segments: unsorted, duplicate, wrong shapes, a wrong ulen", async () => {
    const { hash, ulen } = await pubFixture("d");
    const bad = [
      segment([["fn", enc("b"), [[hash, ulen]]], ["fn", enc("a"), [[hash, ulen]]]]),
      segment([["fn", enc("a"), [[hash, ulen]]], ["fn", enc("a"), [[hash, ulen]]]]),
      encode({ v: 2, entries: [] }),
      encode({ v: 1, entries: [["fn", enc("a"), [[hash.subarray(0, 31), ulen]], new Uint8Array(0)]] }),
      encode({ v: 1, entries: [], extra: 1 }),
    ];
    for (const b of bad) expect((await putSeg("pub", "pubWrite", b)).status).toBe(400);
    expect((await putSeg("pub", "pubWrite", segment([["fn", enc("a"), [[hash, ulen + 1]]]]))).status).toBe(422);
  });

  it("are the one 404 across tenants", async () => {
    const mine = enc("priv blob for a private segment");
    const { hashes } = await putBlobs("privWrite", [mine]);
    const seg = await segOk("priv", "privWrite", segment([["fn", enc("p"), [[hashes[0], mine.length]]]]));
    const absent = await shape(await call("GET", `/t/priv/seg/${"0".repeat(64)}`, { token: "privRead" }));
    expect(absent.status).toBe(404);
    for (const token of [undefined, "otherWrite", "pubWrite"] as const) {
      expect(await shape(await call("GET", `/t/priv/seg/${hex(seg)}`, { token }))).toEqual(absent);
    }
    expect(await shape(await call("GET", `/t/other/seg/${hex(seg)}`, { token: "otherWrite" }))).toEqual(absent);
    expect(await shape(await call("GET", `/t/nosuch/seg/${hex(seg)}`))).toEqual(absent);
  });
});

describe("heads", () => {
  async function ready(tenant: "pub" | "priv" | "other", token: TokenName, label: string) {
    await trusted(tenant, token);
    const content = enc(`blob of ${tenant} ${label}`);
    const { hashes, pack } = await putBlobs(token, [content]);
    const seg = await segOk(tenant, token, segment([["fn", enc(label), [[hashes[0], content.length]]]]));
    return { seg, hash: hashes[0], pack };
  }

  it("writes a new head, reads it back with its ETag, and moves it only to a higher turn under If-Match", async () => {
    const { seg } = await ready("pub", "pubWrite", "h1");
    const name = "p/blaze/9f3c0aa";
    const h1 = await signedHead({ tenant: "pub", name, turn: 1, segments: [seg] });
    const w1 = await cborOf(await putHead("pub", name, "pubWrite", h1));
    expect(w1.refreshed).toEqual({ segments: 0, packs: 0, copied: 0 });
    const [got, none] = await readHeads("pub", [name, "p/blaze/latest"]);
    expect(none).toBeNull();
    expect(got!.etag).toBe(w1.etag);
    expect(got!.head).toEqual(h1);

    expect((await putHead("pub", name, "pubWrite", h1)).status).toBe(412);
    expect((await putHead("pub", name, "pubWrite", h1, {})).status).toBe(428);
    for (const turn of [1, 0]) {
      const r = await putHead("pub", name, "pubWrite", await signedHead({ tenant: "pub", name, turn, segments: [seg] }), { ifMatch: w1.etag });
      expect(r.status).toBe(409);
    }
    const h2 = await signedHead({ tenant: "pub", name, turn: 2, segments: [seg] });
    const w2 = await cborOf(await putHead("pub", name, "pubWrite", h2, { ifMatch: `"${w1.etag}"` }));
    expect((await putHead("pub", name, "pubWrite", await signedHead({ tenant: "pub", name, turn: 3, segments: [seg] }), { ifMatch: w1.etag })).status).toBe(412);
    expect((await readHeads("pub", [name]))[0]!.etag).toBe(w2.etag);
  });

  it("lets exactly one of two concurrent writes from the same ETag through", async () => {
    const { seg } = await ready("pub", "pubWrite", "race");
    const name = "u/" + "ab".repeat(32);
    const w1 = await cborOf(await putHead("pub", name, "pubWrite", await signedHead({ tenant: "pub", name, turn: 1, segments: [seg] })));
    const [a, b] = await Promise.all(
      [2, 3].map(async (turn) => putHead("pub", name, "pubWrite", await signedHead({ tenant: "pub", name, turn, segments: [seg] }), { ifMatch: w1.etag })),
    );
    expect([a.status, b.status].sort()).toEqual([200, 412]);
  });

  it("refuses a head replayed into another tenant or under another name", async () => {
    const { seg } = await ready("priv", "privWrite", "replay");
    await ready("other", "otherWrite", "replay");
    const name = "p/acme/1234567";
    const h = await signedHead({ tenant: "priv", name, turn: 1, segments: [seg] });
    expect((await putHead("priv", name, "privWrite", h)).status).toBe(200);
    const r = await putHead("other", name, "otherWrite", h);
    expect(r.status).toBe(422);
    expect(await r.text()).toContain("signed for tenant priv");
    expect((await putHead("priv", name, "otherWrite", h)).status).toBe(404);
    const moved = await putHead("priv", "p/acme/latest", "privWrite", h);
    expect(moved.status).toBe(422);
    expect(await moved.text()).toContain("signed for p/acme/1234567");
  });

  it("refuses an unknown, revoked, expired or not yet valid key, a created outside its window, and a bad signature", async () => {
    const { seg } = await ready("pub", "pubWrite", "keys");
    const name = (n: number) => `p/keys/${String(n).padStart(7, "0")}`;
    const refused = async (n: number, body: Uint8Array, why: string) => {
      const r = await putHead("pub", name(n), "pubWrite", body);
      expect(r.status).toBe(422);
      expect(await r.text()).toContain(why);
    };
    await refused(1, await signedHead({ tenant: "pub", name: name(1), turn: 1, segments: [seg], signer: ci2 }), "not in the tenant's key list");

    // revoked: the next list drops ci-1
    expect((await putTrust("pub", "pubWrite", await keyList("pub", 2, [{ s: ci2 }]))).status).toBe(200);
    await refused(2, await signedHead({ tenant: "pub", name: name(2), turn: 1, segments: [seg] }), "unknown or revoked");

    expect((await putTrust("pub", "pubWrite", await keyList("pub", 3, [{ s: ci, not_before: now() - 10 * DAY, not_after: now() - 10 }]))).status).toBe(200);
    await refused(3, await signedHead({ tenant: "pub", name: name(3), turn: 1, segments: [seg], created: now() - 20 }), "expired");

    expect((await putTrust("pub", "pubWrite", await keyList("pub", 4, [{ s: ci, not_before: now() + DAY, not_after: now() + 9 * DAY }]))).status).toBe(200);
    await refused(4, await signedHead({ tenant: "pub", name: name(4), turn: 1, segments: [seg] }), "valid from");

    expect((await putTrust("pub", "pubWrite", await keyList("pub", 5, [{ s: ci, not_before: now() - DAY, not_after: now() + DAY }]))).status).toBe(200);
    await refused(5, await signedHead({ tenant: "pub", name: name(5), turn: 1, segments: [seg], created: now() - 2 * DAY }), "outside key ci-1's window");
    await refused(6, await signedHead({ tenant: "pub", name: name(6), turn: 1, segments: [seg], created: now() + 3600 }), "ahead of the gateway's clock");

    const good = decode(await signedHead({ tenant: "pub", name: name(7), turn: 1, segments: [seg] })) as { body: Uint8Array; sig: Uint8Array };
    const sig = good.sig.slice();
    sig[5] ^= 1;
    await refused(7, encode({ body: good.body, sig }), "bad signature");
    // a valid key list signature is not a head signature
    const asKeys = await envelope(ci, KEYS_CONTEXT, good.body);
    await refused(7, asKeys, "bad signature");
    expect((await putHead("pub", name(7), "pubWrite", encode(good))).status).toBe(200);
  });

  it("refuses a head naming a segment the tenant does not have, another tenant's included", async () => {
    const mine = await ready("priv", "privWrite", "segs");
    const theirs = await ready("other", "otherWrite", "segs");
    const name = "p/acme/latest";
    for (const seg of [theirs.seg, await sha(enc("no such segment"))]) {
      const r = await putHead("priv", name, "privWrite", await signedHead({ tenant: "priv", name, turn: 1, segments: [mine.seg, seg] }));
      expect(r.status).toBe(422);
      expect(await r.text()).toContain("which this tenant does not have");
    }
  });

  it("are the one 404 across tenants, and private head names are HMACs", async () => {
    const { seg } = await ready("priv", "privWrite", "hidden");
    const name = "p/acme-internal-fork/abc1234";
    expect((await putHead("priv", name, "privWrite", await signedHead({ tenant: "priv", name, turn: 1, segments: [seg] }))).status).toBe(200);
    expect((await readHeads("priv", [name], "privRead"))[0]).not.toBeNull();
    const body = encode({ names: [name] });
    const absent = await shape(await call("POST", "/t/nosuch/heads", { body }));
    expect(absent.status).toBe(404);
    for (const token of [undefined, "otherWrite", "pubRead"] as const) {
      expect(await shape(await call("POST", "/t/priv/heads", { token, body }))).toEqual(absent);
    }
    expect((await readHeads("other", [name], "otherWrite"))[0]).toBeNull();
    const keys = (await testEnv.BUCKET.list({ prefix: "index/heads/202/" })).objects.map((o) => o.key);
    expect(keys.length).toBe(1);
    expect(keys[0]).toMatch(/^index\/heads\/202\/[0-9a-f]{64}$/);
  });
});

describe("refresh inside head writes", () => {
  it("rewrites a segment over 2 days old and checks its upload date moved; a young one is left alone", async () => {
    await trusted("pub", "pubWrite");
    const { seg } = await pubFixture("old seg");
    const key = `index/seg/101/${hex(seg)}`;
    const before = (await testEnv.BUCKET.head(key))!;
    const name = "p/blaze/latest";
    expect((await cborOf(await putHead("pub", name, "pubWrite", await signedHead({ tenant: "pub", name, turn: 1, segments: [seg] })))).refreshed.segments).toBe(0);
    expect((await testEnv.BUCKET.head(key))!.uploaded.getTime()).toBe(before.uploaded.getTime());

    clock.offsetMs = (2 * DAY + 60) * 1000;
    const etag = (await readHeads("pub", [name]))[0]!.etag;
    const w = await cborOf(await putHead("pub", name, "pubWrite", await signedHead({ tenant: "pub", name, turn: 2, segments: [seg] }), { ifMatch: etag }));
    expect(w.refreshed).toEqual({ segments: 1, packs: 0, copied: 0 });
    const after = (await testEnv.BUCKET.head(key))!;
    expect(after.uploaded.getTime()).toBeGreaterThan(before.uploaded.getTime());
    expect(after.etag).toBe(before.etag);
  });

  it("rewrites a pack over 45 days old, and records the date R2 gives it", async () => {
    await trusted("pub", "pubWrite");
    const { seg, pack } = await pubFixture("old pack");
    const object = `cas/pub/${pack}.bpk`;
    const old = now() - 46 * DAY;
    await testEnv.DB.prepare("UPDATE objects SET uploaded = ?2 WHERE object = ?1").bind(object, old).run();
    const before = (await testEnv.BUCKET.head(object))!;
    await sleep(5);
    const name = "u/" + "cd".repeat(32);
    const w = await cborOf(await putHead("pub", name, "pubWrite", await signedHead({ tenant: "pub", name, turn: 1, segments: [seg] })));
    expect(w.refreshed).toEqual({ segments: 0, packs: 1, copied: 0 });
    const after = (await testEnv.BUCKET.head(object))!;
    expect(after.uploaded.getTime()).toBeGreaterThan(before.uploaded.getTime());
    expect(after.etag).toBe(before.etag);
    const row = await testEnv.DB.prepare("SELECT uploaded FROM objects WHERE object = ?1").bind(object).first<{ uploaded: number }>();
    expect(row!.uploaded).toBe(Math.floor(after.uploaded.getTime() / 1000));
  });

  it("never refreshes a public pack for a private head: it copies the blob into the tenant's namespace", async () => {
    await trusted("priv", "privWrite");
    const pub = await pubFixture("old public dep");
    const object = `cas/pub/${pub.pack}.bpk`;
    const old = now() - 46 * DAY;
    await testEnv.DB.prepare("UPDATE objects SET uploaded = ?2 WHERE object = ?1").bind(object, old).run();
    const before = (await testEnv.BUCKET.head(object))!;
    const seg = await segOk("priv", "privWrite", segment([["unit", enc("dep"), [[pub.hash, pub.ulen]]]]));
    const has = async () => (await cborOf(await call("POST", "/has", { token: "privRead", body: encode({ hashes: [pub.hash] }) }))).state;
    expect(await has()).toEqual([1]);

    const name = "p/acme/latest";
    const w = await cborOf(await putHead("priv", name, "privWrite", await signedHead({ tenant: "priv", name, turn: 1, segments: [seg] })));
    expect(w.refreshed).toEqual({ segments: 0, packs: 0, copied: 1 });
    expect(await has()).toEqual([2]);
    expect((await testEnv.BUCKET.head(object))!.uploaded.getTime()).toBe(before.uploaded.getTime());
    const row = await testEnv.DB.prepare("SELECT uploaded FROM objects WHERE object = ?1").bind(object).first<{ uploaded: number }>();
    expect(row!.uploaded).toBe(old);
    // the copy is the same bytes
    const got = await call("GET", `/blob/${hex(pub.hash)}`, { token: "privRead" });
    expect(new Uint8Array(await got.arrayBuffer())).toEqual(enc("public blob old public dep"));
  });
});

describe("expired locations", () => {
  it("treats a row whose object is over 80 days old as absent, and a write deletes it", async () => {
    const { hashes, pack } = await putBlobs("pubWrite", [enc("about to expire")]);
    const object = `cas/pub/${pack}.bpk`;
    const state = async () => (await cborOf(await call("POST", "/has", { body: encode({ hashes }) }))).state;
    expect(await state()).toEqual([1]);
    await testEnv.DB.prepare("UPDATE objects SET uploaded = ?2 WHERE object = ?1").bind(object, now() - 81 * DAY).run();
    expect(await state()).toEqual([0]);
    expect((await putSeg("pub", "pubWrite", segment([["fn", enc("e"), [[hashes[0], 15]]]]))).status).toBe(422);

    await putBlobs("pubWrite", [enc("an unrelated write")]);
    const rows = await testEnv.DB.prepare("SELECT count(*) AS n FROM blobs WHERE object = ?1").bind(object).first<{ n: number }>();
    expect(rows!.n).toBe(0);
    expect(await testEnv.DB.prepare("SELECT 1 FROM objects WHERE object = ?1").bind(object).first()).toBeNull();
  });
});
