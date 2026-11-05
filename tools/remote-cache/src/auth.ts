// Tokens and tenants live in KV (API-v0.md, "Auth"):
//   token:<sha256(token) hex> -> {"tenant": "<name>", "scope": "read"|"write", "expires": <unix s>|null}
//   tenant:<name>             -> {"id": "<opaque>", "public": bool, "root"?: {"name": "<key name>", "key": "<Ed25519 public key, hex>"}}

export interface TokenRecord {
  tenant: string;
  scope: "read" | "write";
  expires: number | null;
}

export interface TenantRecord {
  id: string;
  public: boolean;
  /** The tenant's root key, which signs its key list (API.md, "Index"). Registered by hand. */
  root?: { name: string; key: string };
}

export type Caller =
  | { kind: "anonymous" }
  | { kind: "token"; tenant: string; scope: "read" | "write" }
  | { kind: "rejected"; reason: string };

/** Revocation is a KV delete; KV reaches every location in about 60 s, and this cache adds at most as much. */
const KV_CACHE_TTL = 60;

export async function sha256Hex(data: string | ArrayBuffer): Promise<string> {
  const bytes = typeof data === "string" ? new TextEncoder().encode(data) : data;
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

export async function identify(request: Request, kv: KVNamespace, nowSeconds: number): Promise<Caller> {
  const header = request.headers.get("authorization");
  if (header === null) return { kind: "anonymous" };
  const m = /^Bearer ([\x21-\x7e]{1,512})$/.exec(header);
  if (!m) return { kind: "rejected", reason: "malformed Authorization header" };
  const record = await kv.get<TokenRecord>(`token:${await sha256Hex(m[1])}`, { type: "json", cacheTtl: KV_CACHE_TTL });
  if (record === null) return { kind: "rejected", reason: "unknown token" };
  if (record.expires !== null && record.expires <= nowSeconds) return { kind: "rejected", reason: "token expired" };
  return { kind: "token", tenant: record.tenant, scope: record.scope };
}

export async function lookupTenant(kv: KVNamespace, tenant: string): Promise<TenantRecord | null> {
  return kv.get<TenantRecord>(`tenant:${tenant}`, { type: "json", cacheTtl: KV_CACHE_TTL });
}
