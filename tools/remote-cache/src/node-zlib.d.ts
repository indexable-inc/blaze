// The part of node:zlib (workerd, nodejs_compat) the gateway uses.
declare module "node:zlib" {
  export function zstdDecompressSync(data: Uint8Array, options?: { maxOutputLength?: number }): Uint8Array;
  export function zstdCompressSync(data: Uint8Array, options?: { params?: Record<number, number> }): Uint8Array;
}
