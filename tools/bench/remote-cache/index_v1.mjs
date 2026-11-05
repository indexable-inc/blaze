#!/usr/bin/env node
// Checks and measures the gateway's signed index (tools/remote-cache/API.md, "Index") from this
// machine, against a private tenant whose root key is registered with the gateway.
//
//     node tools/bench/remote-cache/index_v1.mjs smoke|bench|refresh-prep|refresh-run
//
// Settings come from the environment, so no secret is in an argv: GW (gateway URL), TENANT,
// WRITE_HDR and READ_HDR (files holding `Authorization: Bearer <token>`), ROOT_KEY and CI_KEY
// (files holding an Ed25519 seed in hex; the CI key is named CI_NAME, the root ROOT_NAME), STATE
// (a JSON file kept between commands) and OUT (the TSV rows are appended to).
//
// smoke        registers a key list and runs the index's live checks; exits non-zero on a failure
// bench        segment put/get, head writes without refresh, a heads read of 50 names
// refresh-prep N MIB   puts N packs of MIB each, a segment naming every blob, and a head; writes
//                      the packs' object names to $STATE.objects for the caller to age in D1
// refresh-run LABEL    one head write after the caller aged the packs; records its latency and
//                      what it refreshed

import * as crypto from "node:crypto";
import * as fs from "node:fs";
import { decode, encode } from "../../remote-cache/src/cbor.ts";

const env = process.env;
const GW = env.GW;
const TENANT = env.TENANT;
const auth = (file) => fs.readFileSync(file, "utf8").trim().replace(/^Authorization:\s*/i, "");
const WRITE = auth(env.WRITE_HDR);
const READ = auth(env.READ_HDR);
const HEAD_CONTEXT = Buffer.from("blaze head v1\0");
const KEYS_CONTEXT = Buffer.from("blaze keys v1\0");
const MIB = 1024 * 1024;
const DAY = 86400;

function key(file) {
  const seed = Buffer.from(fs.readFileSync(file, "utf8").trim(), "hex");
  return crypto.createPrivateKey({ key: Buffer.concat([Buffer.from("302e020100300506032b657004220420", "hex"), seed]), format: "der", type: "pkcs8" });
}
const pubOf = (k) => Buffer.from(crypto.createPublicKey(k).export({ format: "jwk" }).x, "base64url");
const ROOT = key(env.ROOT_KEY);
const CI = key(env.CI_KEY);
const sha = (b) => new Uint8Array(crypto.createHash("sha256").update(b).digest());
const hex = (b) => Buffer.from(b).toString("hex");
const now = () => Math.floor(Date.now() / 1000);

function envelope(k, context, body) {
  return encode({ body, sig: new Uint8Array(crypto.sign(null, Buffer.concat([context, body]), k)) });
}

async function call(method, path, { token = WRITE, body, headers = {} } = {}) {
  const h = { ...headers };
  if (token) h.authorization = token;
  const t0 = performance.now();
  const r = await fetch(`${GW}${path}`, { method, headers: h, body });
  const bytes = new Uint8Array(await r.arrayBuffer());
  return { status: r.status, headers: r.headers, bytes, ms: performance.now() - t0, text: () => Buffer.from(bytes).toString() };
}

let failed = 0;
function check(what, cond, detail = "") {
  console.log(`${cond ? "ok  " : "FAIL"} ${what}${cond ? "" : ` ${detail}`}`);
  if (!cond) failed++;
}

const rows = [];
function row(metric, values, unit, detail = "") {
  const s = [...values].sort((a, b) => a - b);
  const q = (p) => s[Math.min(s.length - 1, Math.floor(p * s.length))];
  const line = [metric, q(0.5).toFixed(1), q(0.9).toFixed(1), s[0].toFixed(1), s.at(-1).toFixed(1), unit, String(s.length), detail];
  rows.push(line);
  console.log(line.join("\t"));
}
function flush() {
  if (!env.OUT) return;
  if (!fs.existsSync(env.OUT)) fs.writeFileSync(env.OUT, "metric\tp50\tp90\tmin\tmax\tunit\tn\tdetail\n");
  fs.appendFileSync(env.OUT, rows.map((r) => r.join("\t") + "\n").join(""));
}

const state = () => (fs.existsSync(env.STATE) ? JSON.parse(fs.readFileSync(env.STATE, "utf8")) : {});
const save = (s) => fs.writeFileSync(env.STATE, JSON.stringify(s, null, 1));

// ---- blobs, segments, heads

function stream(contents) {
  const parts = [Buffer.from("BLZSTRM1")];
  for (const c of contents) {
    const h = Buffer.alloc(42);
    h[0] = 1;
    Buffer.from(sha(c)).copy(h, 1);
    h[33] = 1; // raw
    h.writeUInt32LE(c.length, 34);
    h.writeUInt32LE(c.length, 38);
    parts.push(h, c);
  }
  parts.push(Buffer.from([0]));
  return Buffer.concat(parts);
}

async function putBlobs(contents) {
  const body = stream(contents);
  const r = await call("POST", "/put", { body, headers: { "content-length": String(body.length) } });
  if (r.status !== 200) throw new Error(`/put ${r.status} ${r.text()}`);
  return decode(r.bytes).pack;
}

function segment(entries) {
  return encode({ v: 1, entries: entries.map(([kind, k, blobs]) => [kind, k, blobs, new Uint8Array(0)]) });
}

function head({ name, turn, segments, created = now() }) {
  const body = encode({
    tenant: TENANT,
    name,
    turn,
    created,
    versions: { store: 4, codegen: 1 },
    toolchain: "nightly-2026-10-25",
    target: "aarch64-unknown-linux-gnu",
    segments,
    signer: env.CI_NAME,
  });
  return envelope(CI, HEAD_CONTEXT, body);
}

const putHead = (name, body, cond) =>
  call("PUT", `/t/${TENANT}/head/${name}`, { body, headers: cond.etag ? { "if-match": cond.etag } : { "if-none-match": "*" } });

async function putTrust() {
  const body = encode({
    tenant: TENANT,
    turn: now(),
    signer: env.ROOT_NAME,
    keys: [{ name: env.CI_NAME, key: new Uint8Array(pubOf(CI)), role: "heads", not_before: now() - DAY, not_after: now() + 60 * DAY }],
  });
  return call("PUT", `/t/${TENANT}/trust`, { body: envelope(ROOT, KEYS_CONTEXT, body) });
}

async function smallSegment(label) {
  const c = Buffer.from(`index smoke blob ${label} ${crypto.randomUUID()}`);
  await putBlobs([c]);
  const seg = segment([["fn", Buffer.from(label), [[sha(c), c.length]]]]);
  const r = await call("PUT", `/t/${TENANT}/seg`, { body: seg });
  if (r.status !== 200) throw new Error(`seg put ${r.status} ${r.text()}`);
  return { seg, name: decode(r.bytes).seg, ms: r.ms };
}

const randomName = (project) => `p/${project}/${crypto.randomBytes(20).toString("hex")}`;

// ---- commands

async function smoke() {
  const t = await putTrust();
  check("PUT trust signed by the registered root", t.status === 200, `${t.status} ${t.text()}`);
  const own = await call("GET", `/t/${TENANT}/trust`, { token: READ });
  check("GET trust with the read token", own.status === 200);
  const anon = await call("GET", `/t/${TENANT}/trust`, { token: null });
  const absent = await call("GET", `/t/no-such-tenant-x/trust`, { token: null });
  check("GET private trust anonymously is the one 404", anon.status === 404 && anon.text() === absent.text());
  const forged = await call("PUT", `/t/${TENANT}/trust`, {
    body: envelope(CI, KEYS_CONTEXT, encode({ tenant: TENANT, turn: now() + 1, signer: env.ROOT_NAME, keys: [] })),
  });
  check("PUT trust signed by another key is 422", forged.status === 422, String(forged.status));
  const old = await call("PUT", `/t/${TENANT}/trust`, {
    body: envelope(ROOT, KEYS_CONTEXT, encode({ tenant: TENANT, turn: 1, signer: env.ROOT_NAME, keys: [] })),
  });
  check("PUT trust at an older turn is 409", old.status === 409, String(old.status));

  const { seg, name: segName } = await smallSegment("smoke");
  const g = await call("GET", `/t/${TENANT}/seg/${hex(segName)}`, { token: READ });
  check("GET seg returns the bytes put", g.status === 200 && Buffer.from(g.bytes).equals(Buffer.from(seg)));
  const ga = await call("GET", `/t/${TENANT}/seg/${hex(segName)}`, { token: null });
  const gz = await call("GET", `/t/${TENANT}/seg/${"0".repeat(64)}`, { token: READ });
  check("GET private seg anonymously is the 404 an absent one is", ga.status === 404 && gz.status === 404 && ga.text() === gz.text());
  const unreadable = await call("PUT", `/t/${TENANT}/seg`, { body: segment([["fn", Buffer.from("x"), [[sha(Buffer.from("never stored " + crypto.randomUUID())), 5]]]]) });
  check("PUT seg naming an unreadable blob is 422", unreadable.status === 422, String(unreadable.status));

  const hname = randomName("smoke");
  const w1 = await putHead(hname, head({ name: hname, turn: 1, segments: [segName] }), {});
  check("PUT head (new) is 200", w1.status === 200, `${w1.status} ${w1.text()}`);
  const etag1 = decode(w1.bytes).etag;
  const read = await call("POST", `/t/${TENANT}/heads`, { token: READ, body: encode({ names: [hname, randomName("smoke")] }) });
  const got = decode(read.bytes).heads;
  check("POST heads finds it with its ETag, null for an absent one", got[0]?.etag === etag1 && got[1] === null);
  const w2 = await putHead(hname, head({ name: hname, turn: 2, segments: [segName] }), { etag: etag1 });
  check("PUT head at turn 2 under If-Match is 200", w2.status === 200, `${w2.status} ${w2.text()}`);
  const stale = await putHead(hname, head({ name: hname, turn: 3, segments: [segName] }), { etag: etag1 });
  check("PUT head under a stale ETag is 412", stale.status === 412, String(stale.status));
  const older = await putHead(hname, head({ name: hname, turn: 2, segments: [segName] }), { etag: decode(w2.bytes).etag });
  check("PUT head at a turn not above the stored one is 409", older.status === 409, String(older.status));
  const body = decode(head({ name: hname, turn: 9, segments: [segName] }));
  body.sig[0] ^= 1;
  const bad = await putHead(hname, encode(body), { etag: decode(w2.bytes).etag });
  check("PUT head with a bad signature is 422", bad.status === 422, String(bad.status));
  const foreign = await call("PUT", `/t/blaze/head/${hname}`, { body: head({ name: hname, turn: 1, segments: [segName] }), headers: { "if-none-match": "*" } });
  check("PUT head into tenant blaze with this tenant's token is refused", foreign.status === 403, String(foreign.status));
  const other = await call("POST", `/t/${TENANT}/heads`, { token: null, body: encode({ names: [hname] }) });
  const nosuch = await call("POST", `/t/no-such-tenant-x/heads`, { token: null, body: encode({ names: [hname] }) });
  check("POST heads of a private tenant anonymously is the one 404", other.status === 404 && other.text() === nosuch.text());
  const missingSeg = await putHead(randomName("smoke"), head({ name: "x", turn: 1, segments: [sha(Buffer.from("no seg"))] }), {});
  check("PUT head signed for another name is 422", missingSeg.status === 422, String(missingSeg.status));
  if (failed) process.exit(1);
}

async function bench() {
  const n = Number(env.N ?? 20);
  const segPut = [];
  const segGet = [];
  let last;
  for (let i = 0; i < n; i++) {
    last = await smallSegment(`bench ${i}`);
    segPut.push(last.ms);
  }
  for (let i = 0; i < n; i++) segGet.push((await call("GET", `/t/${TENANT}/seg/${hex(last.name)}`, { token: READ })).ms);
  row("seg_put_small", segPut, "ms", "one entry, one blob; includes the D1 readability check");
  row("seg_get_small", segGet, "ms", "R2 read, no edge cache");

  const big = [];
  for (let i = 0; i < 2000; i++) big.push(Buffer.from(`bench big seg blob ${i} ${crypto.randomUUID()}`));
  await putBlobs(big);
  const entries = big.map((c, i) => ["fn", Buffer.from(String(i).padStart(6, "0")), [[sha(c), c.length]]]);
  const bigSeg = segment(entries);
  const bp = [];
  const bg = [];
  let bigName;
  for (let i = 0; i < 5; i++) {
    const r = await call("PUT", `/t/${TENANT}/seg`, { body: bigSeg });
    if (r.status !== 200) throw new Error(`big seg ${r.status} ${r.text()}`);
    bigName = decode(r.bytes).seg;
    bp.push(r.ms);
  }
  for (let i = 0; i < 5; i++) bg.push((await call("GET", `/t/${TENANT}/seg/${hex(bigName)}`, { token: READ })).ms);
  row("seg_put_2000_blobs", bp, "ms", `${bigSeg.length} bytes, 2000 blobs checked in D1 (HMAC keys)`);
  row("seg_get_2000_blobs", bg, "ms", `${bigSeg.length} bytes`);

  const hname = randomName("bench");
  let w = await putHead(hname, head({ name: hname, turn: 1, segments: [last.name] }), {});
  let etag = decode(w.bytes).etag;
  const hw = [];
  const hwBig = [];
  for (let i = 0; i < n; i++) {
    w = await putHead(hname, head({ name: hname, turn: i + 2, segments: [last.name] }), { etag });
    if (w.status !== 200) throw new Error(`head ${w.status} ${w.text()}`);
    etag = decode(w.bytes).etag;
    hw.push(w.ms);
  }
  row("head_put_no_refresh_1seg", hw, "ms", "If-Match, 1 segment of 1 blob, nothing old");
  for (let i = 0; i < 5; i++) {
    w = await putHead(hname, head({ name: hname, turn: n + i + 2, segments: [last.name, bigName] }), { etag });
    if (w.status !== 200) throw new Error(`head ${w.status} ${w.text()}`);
    etag = decode(w.bytes).etag;
    hwBig.push(w.ms);
  }
  row("head_put_no_refresh_2001_blobs", hwBig, "ms", "2 segments naming 2001 blobs, nothing old");

  const names = [];
  for (let i = 0; i < 50; i++) {
    const nm = randomName("bench50");
    const r = await putHead(nm, head({ name: nm, turn: 1, segments: [last.name] }), {});
    if (r.status !== 200) throw new Error(`head ${r.status} ${r.text()}`);
    names.push(nm);
  }
  const hr = [];
  for (let i = 0; i < n; i++) {
    const r = await call("POST", `/t/${TENANT}/heads`, { token: READ, body: encode({ names }) });
    if (r.status !== 200 || decode(r.bytes).heads.some((h) => h === null)) throw new Error(`heads ${r.status}`);
    hr.push(r.ms);
  }
  row("heads_read_50", hr, "ms", "50 present heads, private names (HMAC)");
  const hr0 = [];
  const absent = names.map(() => randomName("absent"));
  for (let i = 0; i < n; i++) hr0.push((await call("POST", `/t/${TENANT}/heads`, { token: READ, body: encode({ names: absent }) })).ms);
  row("heads_read_50_absent", hr0, "ms", "50 absent names");
  flush();
}

async function refreshPrep(count, mib) {
  const objects = [];
  const blobs = [];
  for (let p = 0; p < count; p++) {
    const contents = [];
    for (let left = mib * MIB; left > 0; left -= 4 * MIB) contents.push(crypto.randomBytes(Math.min(4 * MIB, left)));
    objects.push(`cas/t/${env.TENANT_ID}/${await putBlobs(contents)}.bpk`);
    for (const c of contents) blobs.push([sha(c), c.length]);
  }
  const seg = segment(blobs.map((b, i) => ["chunk", Buffer.from(String(i).padStart(6, "0")), [b]]));
  const r = await call("PUT", `/t/${TENANT}/seg`, { body: seg });
  if (r.status !== 200) throw new Error(`seg ${r.status} ${r.text()}`);
  const name = randomName("refresh");
  const w = await putHead(name, head({ name, turn: 1, segments: [decode(r.bytes).seg] }), {});
  if (w.status !== 200) throw new Error(`head ${w.status} ${w.text()}`);
  save({ ...state(), refresh: { name, seg: hex(decode(r.bytes).seg), etag: decode(w.bytes).etag, turn: 1, count, mib } });
  fs.writeFileSync(`${env.STATE}.objects`, objects.join("\n") + "\n");
  console.log(`prepared ${count} packs of ${mib} MiB, head ${name}`);
}

async function refreshRun(label) {
  const s = state();
  const f = s.refresh;
  const w = await putHead(f.name, head({ name: f.name, turn: f.turn + 1, segments: [Buffer.from(f.seg, "hex")] }), { etag: f.etag });
  if (w.status !== 200) throw new Error(`head ${w.status} ${w.text()}`);
  const a = decode(w.bytes);
  f.etag = a.etag;
  f.turn++;
  save(s);
  const timing = /refresh;dur=(\d+)/.exec(w.headers.get("server-timing") ?? "")?.[1];
  console.log(JSON.stringify({ label, ms: w.ms, refresh_ms: Number(timing), refreshed: a.refreshed }));
  const line = [label, w.ms.toFixed(1), timing ?? "", JSON.stringify(a.refreshed), `${f.count} packs of ${f.mib} MiB`];
  if (env.OUT) fs.appendFileSync(`${env.OUT}.refresh`, line.join("\t") + "\n");
}

const [cmd, ...args] = process.argv.slice(2);
if (cmd === "smoke") await smoke();
else if (cmd === "bench") await bench();
else if (cmd === "refresh-prep") await refreshPrep(Number(args[0]), Number(args[1]));
else if (cmd === "refresh-run") await refreshRun(args[0]);
else {
  console.error("usage: index_v1.mjs smoke|bench|refresh-prep N MIB|refresh-run LABEL");
  process.exit(2);
}
