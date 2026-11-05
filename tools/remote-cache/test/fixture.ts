import { env, exports } from "cloudflare:workers";
import { beforeEach } from "vitest";
import { sha256Hex } from "../src/auth";
import type { Env } from "../src/index";

export const testEnv = env as unknown as Env;
export const BASE = "https://gateway.test";

export const TENANTS = {
  pub: { id: "101", public: true },
  priv: { id: "202", public: false },
  other: { id: "303", public: false },
};

export const TOKENS = {
  pubWrite: { token: "t-pub-write", tenant: "pub", scope: "write", expires: null },
  pubRead: { token: "t-pub-read", tenant: "pub", scope: "read", expires: null },
  privWrite: { token: "t-priv-write", tenant: "priv", scope: "write", expires: null },
  privRead: { token: "t-priv-read", tenant: "priv", scope: "read", expires: null },
  otherWrite: { token: "t-other-write", tenant: "other", scope: "write", expires: null },
  expired: { token: "t-expired", tenant: "priv", scope: "write", expires: 1_000_000 },
} as const;

export type TokenName = keyof typeof TOKENS;

beforeEach(async () => {
  for (const [name, record] of Object.entries(TENANTS)) {
    await testEnv.TOKENS.put(`tenant:${name}`, JSON.stringify(record));
  }
  for (const { token, ...record } of Object.values(TOKENS)) {
    await testEnv.TOKENS.put(`token:${await sha256Hex(token)}`, JSON.stringify(record));
  }
});

export interface Opts {
  token?: TokenName;
  body?: Uint8Array | string;
  headers?: Record<string, string>;
}

export function call(method: string, path: string, opts: Opts = {}): Promise<Response> {
  const headers: Record<string, string> = { ...opts.headers };
  if (opts.token) headers.authorization = `Bearer ${TOKENS[opts.token].token}`;
  return (exports as any).default.fetch(new Request(`${BASE}${path}`, { method, headers, body: opts.body }));
}

