// Object names a tenant may use (API-v0.md, "Names"). Anything else is the gateway's one 404.

export type Kind = "pack" | "pack.zst" | "key" | "unit" | "manifest" | "deps";

export interface Name {
  name: string;
  kind: Kind;
  /** The SHA-256 the name commits to, for content-addressed kinds. For `pack.zst` it names the uncompressed pack. */
  hash: string | null;
}

export const HEX64 = /^[0-9a-f]{64}$/;
const TENANT = /^[a-z0-9][a-z0-9-]{0,62}$/;
const PROJECT = /^[A-Za-z0-9._-]{1,128}$/;

export function parseName(name: string): Name | null {
  let m: RegExpExecArray | null;
  if ((m = /^packs\/([0-9a-f]{64})\.pack$/.exec(name))) return { name, kind: "pack", hash: m[1] };
  if ((m = /^packs\/([0-9a-f]{64})\.pack\.zst$/.exec(name))) return { name, kind: "pack.zst", hash: m[1] };
  if ((m = /^keys\/([0-9a-f]{64})$/.exec(name))) return { name, kind: "key", hash: m[1] };
  if ((m = /^units\/([0-9a-f]{64})$/.exec(name))) return { name, kind: "unit", hash: m[1] };
  if ((m = /^(manifests|deps)\/(.*)$/.exec(name))) {
    const project = m[2];
    if (!PROJECT.test(project) || project === "." || project === "..") return null;
    return { name, kind: m[1] === "manifests" ? "manifest" : "deps", hash: null };
  }
  return null;
}

export function validTenant(tenant: string): boolean {
  return TENANT.test(tenant);
}

/** Names whose body R2 itself checks against a SHA-256 on every write. */
export function contentAddressed(kind: Kind): boolean {
  return kind !== "manifest" && kind !== "deps";
}

/** Multipart uploads are for the big objects only. */
export function multipartAllowed(kind: Kind): boolean {
  return kind === "pack" || kind === "pack.zst" || kind === "unit";
}
